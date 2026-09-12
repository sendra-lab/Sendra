//! The model half of sendra-tui's Elm-style architecture: `AppState` and
//! every type it is built from (`LoadState`, `RunState`, edit mode's
//! `EditState`/`EditField`/`TextField`), plus the `Message` enum every state
//! transition flows through. No transition logic lives here — see
//! `super::update` for `update()` itself — this module only defines what the
//! state *is*.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

use sendra_core::{
    ApiKeyAuth, ApiKeyLocation, Assertions, Auth, BasicAuth, CaptureSource, Captures, Document,
    Environment, Method, NotAssertions, OAuthAuth, OAuthGrantType, Request, SendraError,
};

use crate::run_request::RunOutcome;

/// One environment sendra-tui found in `.sendra/environments/`, loaded (not
/// just named) so the overlay can show its variables before it's picked.
#[derive(Debug, Clone)]
pub struct NamedEnvironment {
    pub name: String,
    pub environment: Environment,
}

/// A cursor-addressable text buffer — the minimal text-input primitive
/// every field edit mode touches shares (method, URL and headers as
/// single-line fields; the request body as a multi-line one), rather than
/// each hand-rolling its own insert/delete/cursor-movement logic.
///
/// A dependency like `tui-input` was considered and rejected: every field
/// this crate edits is plain text with no undo/redo, no IME composition,
/// and no need for anything beyond insert/backspace/delete/cursor movement
/// — exactly what this type covers in well under two hundred lines
/// including its own tests. Pulling in an external crate would trade a
/// dependency (plus its own `Input`/event API to learn and wire into
/// `Message`/`update`, and its own opinions on things like scrolling a
/// field wider than its viewport, unneeded here) for something this
/// hand-rolls more simply and keeps fully under this crate's own tests, the
/// same reasoning `body_for_display`'s doc comment gives for reimplementing
/// rather than depending across a crate boundary.
///
/// **Multi-line.** `insert_char`/`backspace`/`delete`/`move_left`/
/// `move_right` were already multi-line-capable without any change: they
/// operate per-`char`, and a `'\n'` is just another `char` to them — typing
/// one inserts a line break, `Backspace` right after one merges the two
/// lines by deleting it like any other character, and so on. Only line-wise
/// concerns needed adding for the body editor, which is genuinely
/// multi-line where method/URL/headers never are: `cursor_row_col` (a
/// multi-line text area's real terminal row/column, where `cursor_chars`
/// alone stopped being enough) and `move_up`/`move_down` (there is no
/// single-line equivalent of moving a cursor vertically). Revisit if a
/// later field genuinely needs more than this still (unicode
/// grapheme-aware cursor movement instead of per-`char`, undo).
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

    /// The cursor's position as `(row, column)`, both zero-based and both in
    /// `char`s — a multi-line text area's real terminal row and column,
    /// where `cursor_chars` (which only ever meant "how far into the one
    /// row") stopped being enough. `row` is how many `'\n'`s precede the
    /// cursor; `column` is `cursor_chars` measured from the start of that
    /// row instead of the start of the whole value.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let before_cursor = &self.value[..self.cursor];
        let row = before_cursor.matches('\n').count();
        let column = before_cursor
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .chars()
            .count();
        (row, column)
    }

    /// Moves the cursor up one line, keeping the same column where the line
    /// above is at least that wide and clamping to its end otherwise — the
    /// same "ragged edge" behavior an ordinary text editor's up/down arrows
    /// have. A no-op on the first line, where there is nowhere up to go.
    pub(super) fn move_up(&mut self) {
        let (row, column) = self.cursor_row_col();
        if row == 0 {
            return;
        }
        self.move_to_row_col(row - 1, column);
    }

    /// The exact mirror of [`Self::move_up`] — a no-op on the last line.
    pub(super) fn move_down(&mut self) {
        let (row, column) = self.cursor_row_col();
        if row >= self.value.matches('\n').count() {
            return;
        }
        self.move_to_row_col(row + 1, column);
    }

    /// Places the cursor at `column` `char`s into line `row` (clamped to
    /// that line's own length), counting lines the same way
    /// `cursor_row_col` does. Shared by `move_up`/`move_down`, the only two
    /// callers that ever need to address a line other than the one the
    /// cursor is already on.
    fn move_to_row_col(&mut self, row: usize, column: usize) {
        let mut offset = 0;
        for (index, line) in self.value.split('\n').enumerate() {
            if index == row {
                let clamped_column = column.min(line.chars().count());
                let byte_offset: usize =
                    line.chars().take(clamped_column).map(char::len_utf8).sum();
                self.cursor = offset + byte_offset;
                return;
            }
            offset += line.len() + 1; // +1 for the '\n' this `split` consumed.
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

/// One editable header row: a key and a value, each its own [`TextField`] so
/// the same insert/backspace/delete/cursor-movement machinery every other
/// edit-mode field already uses applies here too, rather than a second
/// hand-rolled text-input mechanism just for headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderRow {
    pub key: TextField,
    pub value: TextField,
}

impl HeaderRow {
    fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: TextField::new(key),
            value: TextField::new(value),
        }
    }
}

/// Which half of which row currently has focus while editing an
/// environment's variables — the same "row index plus which side" shape
/// `EditField::HeaderKey`/`HeaderValue` already have for a request's
/// headers, pulled out on its own rather than folded into `EditField`
/// itself: an environment-variable edit session is a sibling of a request
/// edit session (see [`EnvironmentEditState`]'s own doc comment), not a mode
/// layered inside the same one, so it needs no other field of `EditField`'s
/// and gains nothing from sharing that type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvVarField {
    Name(usize),
    Value(usize),
}

/// The state of an in-progress edit of one environment's variables, opened
/// from the environment overlay (`Message::EnterEnvironmentEdit`) rather
/// than from the collection browser. A sibling of [`EditState`], not a
/// variant of it: a request's fields and an environment's variables are
/// edited through disjoint UI (the detail pane vs. the overlay), opened and
/// closed by entirely disjoint messages, and `update()`'s own guards already
/// keep the environment overlay and edit mode mutually exclusive (opening
/// one is refused while the other is active) — so the two states can never
/// even coexist, let alone need to share a representation.
///
/// Like [`EditState`], nothing here touches `AppState::environments` until
/// `Message::SaveEnvironmentEdit` actually writes it back — `rows` is a
/// working copy, seeded once by `new` from the real
/// `Environment::variables`, so `Message::CancelEnvironmentEdit` dropping
/// this whole struct is a full, provable no-op no matter what was typed, the
/// exact same guarantee `EditState`'s own doc comment makes for
/// `Message::CancelEdit`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentEditState {
    /// Index into `AppState::environments` this session is editing — fixed
    /// for the life of the session. `Message::SaveEnvironmentEdit` writes
    /// back into `AppState::environments[index]`, never a different one, no
    /// matter what `AppState::environment_overlay`'s own cursor does in the
    /// meantime (it cannot move at all while this session is open — see
    /// `update()`'s own guard).
    pub index: usize,
    /// Working copies of the environment's variables, seeded in the same
    /// order `Environment::variables` (a `BTreeMap`) already iterates —
    /// sorted by name. Reuses [`HeaderRow`] rather than a second hand-rolled
    /// name/value row type: a variable row is exactly the same shape a
    /// request's header row already is.
    pub rows: Vec<HeaderRow>,
    /// Which row (and which half of it) has focus — `None` only when `rows`
    /// is empty, since there is then nothing to focus at all. This is the
    /// zero-variables case this issue's own investigation covers: an
    /// environment can genuinely have no variables (see this module's own
    /// `Environment` doc comment on why that is not an error), and this
    /// type has to represent "editing an environment with nothing in it yet"
    /// cleanly rather than pretend a row exists to focus.
    pub focus: Option<EnvVarField>,
    /// `Some(...)` while a delete confirmation is pending for one of `rows`
    /// — the actual removal happens only on
    /// `Message::ConfirmDeleteEnvVarRow`; `Message::CancelDeleteEnvVarRow`
    /// (or starting any other row action) drops this without touching
    /// `rows` at all. Built on the same shared [`ConfirmPrompt`] every other
    /// destructive-action confirmation in this crate uses, rendered as a
    /// real modal on top of this screen (`view::render_confirm_prompt`)
    /// rather than the inline row annotation this used to be — see this
    /// issue's own scoping notes on why a unified confirmation component
    /// covers this case too now.
    pub pending_delete: Option<PendingEnvVarDelete>,
    /// `Some(message)` right after `Message::SaveEnvironmentEdit` refused to
    /// save — either live validation (an empty or duplicated variable name —
    /// see `update::validate_env_var_rows`) or a real failure writing to
    /// disk. Mirrors `EditState::save_error` exactly: the session stays open
    /// with this set so the typed changes are never lost, only blocked.
    pub save_error: Option<String>,
}

impl EnvironmentEditState {
    /// Visible to `super::update`'s `Message::EnterEnvironmentEdit` arm,
    /// the only place outside this module allowed to start one of these.
    pub(super) fn new(index: usize, environment: &Environment) -> Self {
        let rows: Vec<HeaderRow> = environment
            .variables
            .iter()
            .map(|(name, value)| HeaderRow::new(name.clone(), value.clone()))
            .collect();
        let focus = if rows.is_empty() {
            None
        } else {
            Some(EnvVarField::Name(0))
        };
        Self {
            index,
            rows,
            focus,
            pending_delete: None,
            save_error: None,
        }
    }

    /// The focused `TextField`, or `None` when nothing is focused (`rows` is
    /// empty). Visible to `super::update`'s dispatch for
    /// `Message::EditInsertChar`/`EditBackspace`/`EditDelete`/
    /// `EditCursorLeft`/`EditCursorRight` — the exact same generic
    /// text-field messages a request edit session's own fields already use,
    /// routed here instead of to `AppState::edit_mode` whenever that one is
    /// `None` and this one is `Some` (the two are mutually exclusive — see
    /// this type's own doc comment).
    pub(super) fn focused_field_mut(&mut self) -> Option<&mut TextField> {
        let row = match self.focus? {
            EnvVarField::Name(index) => return self.rows.get_mut(index).map(|row| &mut row.key),
            EnvVarField::Value(index) => index,
        };
        self.rows.get_mut(row).map(|row| &mut row.value)
    }

    /// `Tab`: name → value → next row's name, wrapping from the last row's
    /// value back to the first row's name. A no-op when `rows` is empty.
    pub(super) fn focus_next(&mut self) {
        self.focus = match self.focus {
            None => None,
            Some(EnvVarField::Name(index)) => Some(EnvVarField::Value(index)),
            Some(EnvVarField::Value(index)) => {
                let next = index + 1;
                Some(EnvVarField::Name(if next < self.rows.len() {
                    next
                } else {
                    0
                }))
            }
        };
    }

    /// The exact reverse of [`Self::focus_next`] — `Shift+Tab`.
    pub(super) fn focus_prev(&mut self) {
        self.focus = match self.focus {
            None => None,
            Some(EnvVarField::Name(0)) => {
                Some(EnvVarField::Value(self.rows.len().saturating_sub(1)))
            }
            Some(EnvVarField::Name(index)) => Some(EnvVarField::Value(index - 1)),
            Some(EnvVarField::Value(index)) => Some(EnvVarField::Name(index)),
        };
    }

    /// Appends a new, empty row at the end and moves focus straight to its
    /// name field — mirrors `EditState::add_header_row` exactly. Visible to
    /// `super::update`'s `Message::AddEnvVarRow` arm.
    pub(super) fn add_row(&mut self) {
        self.rows.push(HeaderRow::default());
        self.focus = Some(EnvVarField::Name(self.rows.len() - 1));
    }

    /// Marks whichever row `focus` currently points at pending deletion — a
    /// no-op when nothing is focused. The actual removal is
    /// [`Self::confirm_pending_delete`]; this only opens the confirmation.
    /// Visible to `super::update`'s `Message::RequestDeleteEnvVarRow` arm.
    pub(super) fn request_delete_focused(&mut self) {
        let index = match self.focus {
            Some(EnvVarField::Name(index) | EnvVarField::Value(index)) => index,
            None => return,
        };
        let name = self
            .rows
            .get(index)
            .map(|row| row.key.value())
            .filter(|name| !name.is_empty())
            .unwrap_or("(unnamed)");
        self.pending_delete = Some(PendingEnvVarDelete {
            index,
            prompt: ConfirmPrompt::new(format!("Delete variable '{name}'? This cannot be undone.")),
        });
    }

    /// Drops a pending deletion without removing anything — visible to
    /// `super::update`'s `Message::CancelDeleteEnvVarRow` arm.
    pub(super) fn cancel_pending_delete(&mut self) {
        self.pending_delete = None;
    }

    /// Actually removes the row named by `pending_delete`, if any — visible
    /// to `super::update`'s `Message::ConfirmDeleteEnvVarRow` arm. Focus
    /// afterward never dangles on a removed row: it moves to the row that
    /// slid into its place (or the new last row, if the deleted one was
    /// last), or to `None` if no rows remain — the zero-variables state this
    /// type's own `focus` doc comment describes.
    pub(super) fn confirm_pending_delete(&mut self) {
        let Some(pending) = self.pending_delete.take() else {
            return;
        };
        let index = pending.index;
        if index >= self.rows.len() {
            return;
        }
        self.rows.remove(index);
        self.focus = if self.rows.is_empty() {
            None
        } else {
            Some(EnvVarField::Name(index.min(self.rows.len() - 1)))
        };
    }
}

/// What `Message::RequestDeleteEnvVarRow` opens and
/// `Message::ConfirmDeleteEnvVarRow`/`Message::CancelDeleteEnvVarRow` close —
/// which row is pending deletion, plus the shared confirmation UI itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEnvVarDelete {
    /// Index into `EnvironmentEditState::rows` — fixed for the life of this
    /// prompt.
    pub index: usize,
    pub prompt: ConfirmPrompt,
}

/// Which auth sub-field currently has focus, when the selected request's
/// `auth:` block is one this edit session can reach. Which of these are ever
/// reachable at once depends entirely on which of `bearer`/`basic`/
/// `api_key`/`oauth` the request actually has — see `AuthEdit::field_order`,
/// which is the only thing that decides that; a bearer-only request never
/// makes `BasicUser` reachable, for instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthField {
    BearerToken,
    BasicUser,
    BasicPass,
    ApiKeyName,
    ApiKeyValue,
    ApiKeyLocation,
    OAuthGrantType,
    OAuthTokenUrl,
    OAuthClientId,
    OAuthClientSecret,
    OAuthScope,
    OAuthUsername,
    OAuthPassword,
}

/// Which of edit mode's fields `Message::EditFocusNext`/`EditFocusPrev`
/// (`Tab`/`Shift+Tab`) is currently pointed at, and so which one
/// `Message::EditInsertChar`/`EditBackspace`/etc. act on. `HeaderKey(i)`/
/// `HeaderValue(i)` index into `EditState::headers`; `Body` is the raw body
/// text area (see `BodyEdit`) and only ever appears in the focus cycle when
/// `EditState::body` is actually editable — see `next`/`prev`'s own
/// `has_body` parameter. `Auth(field)` is the same idea for `EditState::auth`
/// — see `next`/`prev`'s `auth_fields` parameter and `AuthEdit::field_order`.
/// `AssertionPath(i)`/`AssertionOperator(i)`/`AssertionValue(i)`/
/// `AssertionNegate(i)` index into `EditState::assertions` — see
/// `EditState::assertion_row_count` and `AssertionRow`'s own doc comment for
/// what one row covers. `CaptureName(i)`/`CaptureKind(i)`/`CaptureValue(i)`
/// index into `EditState::captures` — see `EditState::capture_row_count` and
/// `CaptureRow`'s own doc comment.
///
/// `Name` is first in the cycle (and the default focus a fresh edit session
/// opens on) — a request's identity comes before what it does. Unlike
/// `Method`, an empty `Name` is never rejected here the way `method_error`
/// rejects a bad method: `Request::name` is a plain `Option<String>`, valid
/// empty or not, and *whether* a name is required at all depends on context
/// this `EditField`/`EditState` don't have (a bare request needs none; one
/// inside a collection needs a real, unique one) — see
/// `sendra_core::Document::validate`, which `Message::SaveEdit` relies on
/// `Document::save_to_path` to check instead of duplicating that rule here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EditField {
    #[default]
    Name,
    Method,
    Url,
    HeaderKey(usize),
    HeaderValue(usize),
    Body,
    Auth(AuthField),
    AssertionPath(usize),
    AssertionOperator(usize),
    AssertionValue(usize),
    AssertionNegate(usize),
    CaptureName(usize),
    CaptureKind(usize),
    CaptureValue(usize),
}

impl EditField {
    /// Visible to `super::update`'s `Message::EditFocusNext` arm. Takes
    /// `header_count`, `has_body`, `auth_fields` and `assertion_row_count`
    /// (rather than being a pure function of `self` alone) since whether
    /// `Url` steps straight into the first header row, `Body`, the first
    /// auth field, or the first assertion row, and where the cycle wraps
    /// back to `Method` from, all depend on how many header/assertion rows
    /// currently exist, whether there is an editable body field at all
    /// right now (see `BodyEdit::Unsupported`, which has no field to
    /// focus), which auth fields (if any) `EditState::auth` currently
    /// offers (see `AuthEdit::field_order`, empty for a request with no
    /// `auth:` at all), and how many assertion/capture rows currently exist.
    /// Order: Name → Method → URL → each header row's key then value, in
    /// index order → Body (if editable) → each auth field, in order → each
    /// assertion row's path, operator, value then negate flag, in index
    /// order → each capture row's name, kind then value, in index order →
    /// back to Name.
    pub(super) fn next(
        self,
        header_count: usize,
        has_body: bool,
        auth_fields: &[AuthField],
        assertion_row_count: usize,
        capture_row_count: usize,
    ) -> Self {
        match self {
            EditField::Name => EditField::Method,
            EditField::Method => EditField::Url,
            EditField::Url => {
                if header_count > 0 {
                    EditField::HeaderKey(0)
                } else {
                    Self::after_headers(
                        has_body,
                        auth_fields,
                        assertion_row_count,
                        capture_row_count,
                    )
                }
            }
            EditField::HeaderKey(index) => EditField::HeaderValue(index),
            EditField::HeaderValue(index) => {
                if index + 1 < header_count {
                    EditField::HeaderKey(index + 1)
                } else {
                    Self::after_headers(
                        has_body,
                        auth_fields,
                        assertion_row_count,
                        capture_row_count,
                    )
                }
            }
            EditField::Body => {
                Self::after_body(auth_fields, assertion_row_count, capture_row_count)
            }
            EditField::Auth(field) => {
                let index = auth_fields.iter().position(|candidate| *candidate == field);
                match index.and_then(|index| auth_fields.get(index + 1)) {
                    Some(&next_field) => EditField::Auth(next_field),
                    None => Self::after_auth(assertion_row_count, capture_row_count),
                }
            }
            EditField::AssertionPath(index) => EditField::AssertionOperator(index),
            EditField::AssertionOperator(index) => EditField::AssertionValue(index),
            EditField::AssertionValue(index) => EditField::AssertionNegate(index),
            EditField::AssertionNegate(index) => {
                if index + 1 < assertion_row_count {
                    EditField::AssertionPath(index + 1)
                } else {
                    Self::after_assertions(capture_row_count)
                }
            }
            EditField::CaptureName(index) => EditField::CaptureKind(index),
            EditField::CaptureKind(index) => EditField::CaptureValue(index),
            EditField::CaptureValue(index) => {
                if index + 1 < capture_row_count {
                    EditField::CaptureName(index + 1)
                } else {
                    EditField::Name
                }
            }
        }
    }

    /// What comes right after the last header row (or straight after `Url`,
    /// with no headers at all): `Body` if it's editable, otherwise whatever
    /// `after_body` says — shared by the `Url` and `HeaderValue` arms of
    /// [`Self::next`], which both reach this exact same decision.
    fn after_headers(
        has_body: bool,
        auth_fields: &[AuthField],
        assertion_row_count: usize,
        capture_row_count: usize,
    ) -> Self {
        if has_body {
            EditField::Body
        } else {
            Self::after_body(auth_fields, assertion_row_count, capture_row_count)
        }
    }

    /// What comes right after `Body` (or straight after headers/`Url`, with
    /// no editable body): the first auth field, if there is one, otherwise
    /// whatever `after_auth` says.
    fn after_body(
        auth_fields: &[AuthField],
        assertion_row_count: usize,
        capture_row_count: usize,
    ) -> Self {
        match auth_fields.first() {
            Some(&first) => EditField::Auth(first),
            None => Self::after_auth(assertion_row_count, capture_row_count),
        }
    }

    /// What comes right after the last auth field (or straight after
    /// headers/`Url`/`Body`, with no auth fields at all): the first
    /// assertion row's path, if there is one, otherwise whatever
    /// `after_assertions` says.
    fn after_auth(assertion_row_count: usize, capture_row_count: usize) -> Self {
        if assertion_row_count > 0 {
            EditField::AssertionPath(0)
        } else {
            Self::after_assertions(capture_row_count)
        }
    }

    /// What comes right after the last assertion row (or straight after
    /// auth/headers/`Url`/`Body`, with no assertion rows at all): the first
    /// capture row's name, if there is one, otherwise wrapping back to
    /// `Name`.
    fn after_assertions(capture_row_count: usize) -> Self {
        if capture_row_count > 0 {
            EditField::CaptureName(0)
        } else {
            EditField::Name
        }
    }

    /// The exact reverse of [`Self::next`] — visible to `super::update`'s
    /// `Message::EditFocusPrev` arm (`Shift+Tab`).
    pub(super) fn prev(
        self,
        header_count: usize,
        has_body: bool,
        auth_fields: &[AuthField],
        assertion_row_count: usize,
        capture_row_count: usize,
    ) -> Self {
        match self {
            EditField::Name => Self::before_name(
                header_count,
                has_body,
                auth_fields,
                assertion_row_count,
                capture_row_count,
            ),
            EditField::Method => EditField::Name,
            EditField::Url => EditField::Method,
            EditField::HeaderKey(0) => EditField::Url,
            EditField::HeaderKey(index) => EditField::HeaderValue(index - 1),
            EditField::HeaderValue(index) => EditField::HeaderKey(index),
            EditField::Body => {
                if header_count > 0 {
                    EditField::HeaderValue(header_count - 1)
                } else {
                    EditField::Url
                }
            }
            EditField::Auth(field) => {
                match auth_fields.iter().position(|candidate| *candidate == field) {
                    Some(0) | None => Self::before_auth(header_count, has_body),
                    Some(index) => EditField::Auth(auth_fields[index - 1]),
                }
            }
            EditField::AssertionPath(0) => {
                Self::before_assertions(header_count, has_body, auth_fields)
            }
            EditField::AssertionPath(index) => EditField::AssertionNegate(index - 1),
            EditField::AssertionOperator(index) => EditField::AssertionPath(index),
            EditField::AssertionValue(index) => EditField::AssertionOperator(index),
            EditField::AssertionNegate(index) => EditField::AssertionValue(index),
            EditField::CaptureName(0) => {
                Self::before_captures(header_count, has_body, auth_fields, assertion_row_count)
            }
            EditField::CaptureName(index) => EditField::CaptureValue(index - 1),
            EditField::CaptureKind(index) => EditField::CaptureName(index),
            EditField::CaptureValue(index) => EditField::CaptureKind(index),
        }
    }

    /// What comes right before the first auth field: `Body`, if it's
    /// editable; otherwise the last header row's value, if there are any
    /// headers; otherwise `Url`. Shared by `Auth(field)`'s own `prev` arm
    /// (when `field` is the first auth field) and by
    /// [`Self::before_assertions`] (when there is no auth field at all).
    fn before_auth(header_count: usize, has_body: bool) -> Self {
        if has_body {
            EditField::Body
        } else if header_count > 0 {
            EditField::HeaderValue(header_count - 1)
        } else {
            EditField::Url
        }
    }

    /// What comes right before the first assertion row: the last auth
    /// field, if there is one; otherwise whatever [`Self::before_auth`]
    /// says. Shared by `AssertionPath(0)`'s own `prev` arm and by
    /// [`Self::before_method`] (when there are no assertion rows at all).
    fn before_assertions(header_count: usize, has_body: bool, auth_fields: &[AuthField]) -> Self {
        match auth_fields.last() {
            Some(&last) => EditField::Auth(last),
            None => Self::before_auth(header_count, has_body),
        }
    }

    /// What comes right before the first capture row: the last assertion
    /// row's negate flag, if there are any assertion rows; otherwise
    /// whatever [`Self::before_assertions`] says. Shared by `CaptureName(0)`'s
    /// own `prev` arm and by [`Self::before_method`] (when there are no
    /// capture rows at all).
    fn before_captures(
        header_count: usize,
        has_body: bool,
        auth_fields: &[AuthField],
        assertion_row_count: usize,
    ) -> Self {
        if assertion_row_count > 0 {
            EditField::AssertionNegate(assertion_row_count - 1)
        } else {
            Self::before_assertions(header_count, has_body, auth_fields)
        }
    }

    /// What comes right before `Name` when the cycle wraps backward: the
    /// last capture row's value field, if there are any capture rows;
    /// otherwise whatever [`Self::before_captures`] says. The exact mirror
    /// of how [`Self::after_headers`]/[`Self::after_body`]/
    /// [`Self::after_auth`]/[`Self::after_assertions`] decide what comes
    /// *after* those same sections going forward.
    fn before_name(
        header_count: usize,
        has_body: bool,
        auth_fields: &[AuthField],
        assertion_row_count: usize,
        capture_row_count: usize,
    ) -> Self {
        if capture_row_count > 0 {
            EditField::CaptureValue(capture_row_count - 1)
        } else {
            Self::before_captures(header_count, has_body, auth_fields, assertion_row_count)
        }
    }
}

/// How the selected request's body participates in edit mode — decided once
/// by `BodyEdit::new` from whichever of the five body-bearing fields
/// (`body`/`json`/`body_file`/`form`/`multipart`) the request actually has
/// set (`Request::validate` guarantees at most one), and unchanged for the
/// rest of the edit session even if what gets typed would also fit a
/// different shape (see `Editable`'s own doc comment).
///
/// **Scoping decision for this issue**: only a request with no body, a
/// plain `body:`, or a `json:` body gets a real editor here — the three
/// remaining shapes (`body_file`, `form`, `multipart`) are shown read-only
/// (`Unsupported`, surfaced by `render_edit_pane` as plain text, never a
/// text area) rather than partially or fully editable. This is a deliberate
/// line, not a gap that slipped through: `body_file` names a file on disk
/// that some other tool may already have open, editing it from inside
/// sendra-tui would mean either silently overwriting that file on save (a
/// surprising side effect for a "request" edit) or inventing a separate
/// "detach from the file" step this issue was never asked to design; `form`
/// and `multipart` are structured (name/value pairs, and file parts for the
/// latter) and would need their own list-of-rows editor in the shape of
/// `HeaderRow`'s, which is a real feature in its own right, not a natural
/// fit for a raw text area. Both are honest gaps to revisit as their own
/// issues, not silently dropped: `Unsupported`'s `description` is exactly
/// what tells the user, in the pane itself, that this body exists but isn't
/// editable here.
#[derive(Debug, Clone, PartialEq)]
pub enum BodyEdit {
    // `Editable`/`Unsupported` documented below; `Default` is implemented
    // manually (an empty, plain-text `Editable` — the same "no body"
    // starting point `EditState::new` builds for a request with none) only
    // so `#[derive(Default)]` on `EditState` itself has something to build
    // from before any real request is loaded.
    /// A `body:`/`json:` body, or no body at all — editable as raw text in
    /// one shared text area. `is_json` remembers which of `Request::body`/
    /// `Request::json` `Message::SaveEdit` writes the parsed text back
    /// into: `true` for a request that had `json:` set (the text starts out
    /// pretty-printed from that value, and is parsed back into JSON on
    /// save — see `validate_body_for_save`/`apply_body_edit` in `update`),
    /// `false` for `body:` or no body at all (written back as plain text,
    /// never parsed or validated). Fixed for the whole edit session: typing
    /// JSON-shaped text into a plain-body request does not switch it into
    /// JSON mode (which would surprise a user who never asked for
    /// validation), and typing non-JSON text into a `json:` body's editor
    /// is exactly the invalid-JSON case `Message::SaveEdit` is meant to
    /// catch, not a silent fallback to a plain string.
    Editable { text: TextField, is_json: bool },
    /// `body_file`, `form`, or `multipart` — not editable through this
    /// text area; see this enum's own doc comment for why. `description`
    /// is exactly what `render_edit_pane` shows in place of a text area.
    Unsupported { description: String },
}

impl Default for BodyEdit {
    fn default() -> Self {
        BodyEdit::Editable {
            text: TextField::default(),
            is_json: false,
        }
    }
}

impl BodyEdit {
    /// Visible to `super::update`'s `Message::EnterEditMode` arm via
    /// `EditState::new`.
    fn new(request: &Request) -> Self {
        if let Some(path) = &request.body_file {
            return BodyEdit::Unsupported {
                description: format!(
                    "body_file: {path} (not editable here — edit the file itself, then re-open this request)"
                ),
            };
        }
        if !request.form.is_empty() {
            return BodyEdit::Unsupported {
                description: format!(
                    "form body ({} field{}) — not editable here",
                    request.form.len(),
                    if request.form.len() == 1 { "" } else { "s" }
                ),
            };
        }
        if !request.multipart.is_empty() {
            return BodyEdit::Unsupported {
                description: format!(
                    "multipart body ({} part{}) — not editable here",
                    request.multipart.len(),
                    if request.multipart.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
            };
        }
        if let Some(value) = &request.json {
            let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            return BodyEdit::Editable {
                text: TextField::new(text),
                is_json: true,
            };
        }
        BodyEdit::Editable {
            text: TextField::new(request.body.clone().unwrap_or_default()),
            is_json: false,
        }
    }
}

/// The state of an in-progress edit of `Request::auth` — a working copy
/// shaped by whichever of `bearer`/`basic`/`api_key`/`oauth` the request
/// actually has. `None` covers a request with no `auth:` block at all; this
/// edit session never introduces one where there wasn't one already (no
/// "pick an auth type" step exists here — see this issue's own scoping
/// notes). Seeded once by `AuthEdit::new` and converted back by `to_auth` —
/// the same round-trip `BodyEdit`/`HeaderRow` already go through for their
/// own fields.
///
/// **OAuth scoping decision.** `grant_type`/`token_url`/`client_id`/
/// `client_secret`/`scope`/`username`/`password` are edited here as plain
/// fields, the same as bearer/basic/api_key — they are static configuration
/// sendra-core sends *to* the token endpoint, not the token-acquisition flow
/// itself (`Request::resolve_oauth`, a real network call this never
/// triggers). What this deliberately does not do: fetch a token to preview
/// it, or validate these credentials against the real endpoint — editing
/// only changes what the next real run would send there. See
/// `render_edit_pane`'s own note on why the live "resolved auth" preview
/// skips OAuth specifically, the one place this scoping decision is visible
/// in the UI itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum AuthEdit {
    #[default]
    None,
    Bearer {
        token: TextField,
    },
    Basic {
        user: TextField,
        pass: TextField,
    },
    ApiKey {
        name: TextField,
        value: TextField,
        location: ApiKeyLocation,
    },
    OAuth {
        grant_type: OAuthGrantType,
        token_url: TextField,
        client_id: TextField,
        client_secret: TextField,
        scope: TextField,
        username: TextField,
        password: TextField,
    },
}

impl AuthEdit {
    /// Visible to `super::update`'s `Message::EnterEditMode` arm via
    /// `EditState::new`. `Auth::validate_exclusivity` guarantees at most one
    /// of `bearer`/`basic`/`api_key`/`oauth` is set on any `Auth` that made
    /// it into a loaded `Request`, so checking them in a fixed order finds
    /// whichever one it is without needing to know which in advance.
    fn new(auth: Option<&Auth>) -> Self {
        let Some(auth) = auth else {
            return AuthEdit::None;
        };
        if let Some(token) = &auth.bearer {
            return AuthEdit::Bearer {
                token: TextField::new(token.clone()),
            };
        }
        if let Some(basic) = &auth.basic {
            return AuthEdit::Basic {
                user: TextField::new(basic.user.clone()),
                pass: TextField::new(basic.pass.clone()),
            };
        }
        if let Some(api_key) = &auth.api_key {
            return AuthEdit::ApiKey {
                name: TextField::new(api_key.name.clone()),
                value: TextField::new(api_key.value.clone()),
                location: api_key.r#in,
            };
        }
        if let Some(oauth) = &auth.oauth {
            return AuthEdit::OAuth {
                grant_type: oauth.grant_type,
                token_url: TextField::new(oauth.token_url.clone()),
                client_id: TextField::new(oauth.client_id.clone()),
                client_secret: TextField::new(oauth.client_secret.clone()),
                scope: TextField::new(oauth.scope.clone().unwrap_or_default()),
                username: TextField::new(oauth.username.clone().unwrap_or_default()),
                password: TextField::new(oauth.password.clone().unwrap_or_default()),
            };
        }
        AuthEdit::None
    }

    /// The exact reverse of `new` — visible to `super::update::EditState::to_request`,
    /// which `Message::SaveEdit` and `render_edit_pane`'s live auth preview
    /// both build on. `scope`/`username`/`password` round-trip to `None`
    /// when left blank, matching how `OAuthAuth`'s own fields are optional.
    pub(super) fn to_auth(&self) -> Option<Auth> {
        match self {
            AuthEdit::None => None,
            AuthEdit::Bearer { token } => Some(Auth {
                bearer: Some(token.value().to_string()),
                basic: None,
                api_key: None,
                oauth: None,
            }),
            AuthEdit::Basic { user, pass } => Some(Auth {
                bearer: None,
                basic: Some(BasicAuth {
                    user: user.value().to_string(),
                    pass: pass.value().to_string(),
                }),
                api_key: None,
                oauth: None,
            }),
            AuthEdit::ApiKey {
                name,
                value,
                location,
            } => Some(Auth {
                bearer: None,
                basic: None,
                api_key: Some(ApiKeyAuth {
                    r#in: *location,
                    name: name.value().to_string(),
                    value: value.value().to_string(),
                }),
                oauth: None,
            }),
            AuthEdit::OAuth {
                grant_type,
                token_url,
                client_id,
                client_secret,
                scope,
                username,
                password,
            } => Some(Auth {
                bearer: None,
                basic: None,
                api_key: None,
                oauth: Some(OAuthAuth {
                    grant_type: *grant_type,
                    token_url: token_url.value().to_string(),
                    client_id: client_id.value().to_string(),
                    client_secret: client_secret.value().to_string(),
                    scope: non_empty(scope.value()),
                    username: non_empty(username.value()),
                    password: non_empty(password.value()),
                }),
            }),
        }
    }

    /// Which `AuthField`s are reachable right now, in focus-cycle order —
    /// empty for `AuthEdit::None`, since there is nothing to focus. What
    /// `EditField::next`/`prev` and `EditState::has_auth` build on,
    /// mirroring `BodyEdit::has_editable_body`'s role for the body field.
    pub(super) fn field_order(&self) -> &'static [AuthField] {
        match self {
            AuthEdit::None => &[],
            AuthEdit::Bearer { .. } => &[AuthField::BearerToken],
            AuthEdit::Basic { .. } => &[AuthField::BasicUser, AuthField::BasicPass],
            AuthEdit::ApiKey { .. } => &[
                AuthField::ApiKeyName,
                AuthField::ApiKeyValue,
                AuthField::ApiKeyLocation,
            ],
            AuthEdit::OAuth { .. } => &[
                AuthField::OAuthGrantType,
                AuthField::OAuthTokenUrl,
                AuthField::OAuthClientId,
                AuthField::OAuthClientSecret,
                AuthField::OAuthScope,
                AuthField::OAuthUsername,
                AuthField::OAuthPassword,
            ],
        }
    }

    /// The focused `TextField` for `field`, or `None` when `field` names a
    /// fixed-enum sub-field (`ApiKeyLocation`/`OAuthGrantType`) that has no
    /// `TextField` at all — see `AuthEdit::toggle` for how those are
    /// changed instead. Visible to `EditState::focused_field_mut`.
    pub(super) fn text_field_mut(&mut self, field: AuthField) -> Option<&mut TextField> {
        match (self, field) {
            (AuthEdit::Bearer { token }, AuthField::BearerToken) => Some(token),
            (AuthEdit::Basic { user, .. }, AuthField::BasicUser) => Some(user),
            (AuthEdit::Basic { pass, .. }, AuthField::BasicPass) => Some(pass),
            (AuthEdit::ApiKey { name, .. }, AuthField::ApiKeyName) => Some(name),
            (AuthEdit::ApiKey { value, .. }, AuthField::ApiKeyValue) => Some(value),
            (AuthEdit::ApiKey { .. }, AuthField::ApiKeyLocation) => None,
            (AuthEdit::OAuth { .. }, AuthField::OAuthGrantType) => None,
            (AuthEdit::OAuth { token_url, .. }, AuthField::OAuthTokenUrl) => Some(token_url),
            (AuthEdit::OAuth { client_id, .. }, AuthField::OAuthClientId) => Some(client_id),
            (AuthEdit::OAuth { client_secret, .. }, AuthField::OAuthClientSecret) => {
                Some(client_secret)
            }
            (AuthEdit::OAuth { scope, .. }, AuthField::OAuthScope) => Some(scope),
            (AuthEdit::OAuth { username, .. }, AuthField::OAuthUsername) => Some(username),
            (AuthEdit::OAuth { password, .. }, AuthField::OAuthPassword) => Some(password),
            _ => unreachable!(
                "focus is never Auth(field) for a field outside this AuthEdit's own \
                 field_order — see EditField::next/prev"
            ),
        }
    }

    /// Toggles whichever fixed-enum sub-field `field` names
    /// (`api_key.in`/`oauth.grant_type`) — a no-op (returning `false`) for
    /// any text field, or if `field` doesn't belong to this `AuthEdit`'s own
    /// variant. `Message::EditCursorLeft`/`EditCursorRight` (`Left`/`Right`)
    /// call this before falling back to ordinary cursor movement, since
    /// these two fields have no `TextField`/cursor to move through at all —
    /// see `super::update::edit_move_or_toggle`.
    pub(super) fn toggle(&mut self, field: AuthField) -> bool {
        match (self, field) {
            (AuthEdit::ApiKey { location, .. }, AuthField::ApiKeyLocation) => {
                *location = match location {
                    ApiKeyLocation::Header => ApiKeyLocation::Query,
                    ApiKeyLocation::Query => ApiKeyLocation::Header,
                };
                true
            }
            (AuthEdit::OAuth { grant_type, .. }, AuthField::OAuthGrantType) => {
                *grant_type = match grant_type {
                    OAuthGrantType::ClientCredentials => OAuthGrantType::Password,
                    OAuthGrantType::Password => OAuthGrantType::ClientCredentials,
                };
                true
            }
            _ => false,
        }
    }
}

/// `Some(text)` unless `text` is empty — what `AuthEdit::to_auth` uses for
/// `OAuthAuth`'s optional `scope`/`username`/`password`, so clearing one of
/// these fields back to blank saves as the field being genuinely absent
/// again, not present-but-empty. Also visible to `super::update`, which
/// applies the same convention to `EditState::name` when writing it back
/// into `Request::name`.
pub(super) fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}

/// Cycles `value` forward through the fixed, closed set `all` (wrapping past
/// the last entry back to the first) — the shared shell behind
/// `JsonOperator::next`/`CaptureKind::next`, which differ only in which
/// closed set they cycle through. `value` not found in `all` at all (never
/// happens in practice — every caller passes its own variant) falls back to
/// index 0, the same permissive default the pre-extraction code already had.
fn cycle_next<T: Copy + PartialEq>(value: T, all: &[T]) -> T {
    let index = all.iter().position(|item| *item == value).unwrap_or(0);
    all[(index + 1) % all.len()]
}

/// The exact reverse of [`cycle_next`] — the shared shell behind
/// `JsonOperator::prev`/`CaptureKind::prev`.
fn cycle_prev<T: Copy + PartialEq>(value: T, all: &[T]) -> T {
    let index = all.iter().position(|item| *item == value).unwrap_or(0);
    all[(index + all.len() - 1) % all.len()]
}

/// Which operator a `json:` path assertion compares with — the fixed, closed
/// set sendra-core's own (private) `JsonSpec::parse` recognises as a
/// single-key operator object, plus bare equality. A real, closed
/// vocabulary, so — like `ApiKeyLocation`/`OAuthGrantType` before it — this
/// is cycled through (`Message::EditCursorLeft`/`EditCursorRight`), never
/// typed as free text. See `sendra_core::assertions::json`'s module docs for
/// exactly what each one checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JsonOperator {
    #[default]
    Equals,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    Contains,
    Length,
    Matches,
}

impl JsonOperator {
    const ALL: [JsonOperator; 8] = [
        JsonOperator::Equals,
        JsonOperator::GreaterThan,
        JsonOperator::GreaterThanOrEqual,
        JsonOperator::LessThan,
        JsonOperator::LessThanOrEqual,
        JsonOperator::Contains,
        JsonOperator::Length,
        JsonOperator::Matches,
    ];

    /// Visible to `EditState::toggle_focused`, which calls this for `Right`
    /// and [`Self::prev`] for `Left` — an ordinary forward/backward cycle
    /// through [`Self::ALL`], wrapping at either end.
    pub(super) fn next(self) -> Self {
        cycle_next(self, &Self::ALL)
    }

    pub(super) fn prev(self) -> Self {
        cycle_prev(self, &Self::ALL)
    }

    /// The one-key operator name sendra-core's real YAML schema uses
    /// (`greater_than`, `matches`, ...) — shown in the edit pane so what is
    /// on screen matches what the file would say, and what
    /// [`Self::wrap`]/[`Self::detect`] build and read back.
    pub(super) fn label(self) -> &'static str {
        match self {
            JsonOperator::Equals => "equals",
            JsonOperator::GreaterThan => "greater_than",
            JsonOperator::GreaterThanOrEqual => "greater_than_or_equal",
            JsonOperator::LessThan => "less_than",
            JsonOperator::LessThanOrEqual => "less_than_or_equal",
            JsonOperator::Contains => "contains",
            JsonOperator::Length => "length",
            JsonOperator::Matches => "matches",
        }
    }

    /// Builds the real `json:` value sendra-core's own `JsonSpec::parse`
    /// (`sendra_core::assertions::json`, private to that crate) would read
    /// back as this operator: `arg` unwrapped for `Equals`, or a one-key
    /// object (`{greater_than: arg}`, ...) for every named operator. This is
    /// the only place sendra-tui encodes the operator convention — it never
    /// reimplements what a value *means*, only how to spell it, and
    /// `Assertions::evaluate` is what actually interprets it afterward.
    pub(super) fn wrap(self, arg: serde_json::Value) -> serde_json::Value {
        match self {
            JsonOperator::Equals => arg,
            JsonOperator::GreaterThan => serde_json::json!({ "greater_than": arg }),
            JsonOperator::GreaterThanOrEqual => {
                serde_json::json!({ "greater_than_or_equal": arg })
            }
            JsonOperator::LessThan => serde_json::json!({ "less_than": arg }),
            JsonOperator::LessThanOrEqual => serde_json::json!({ "less_than_or_equal": arg }),
            JsonOperator::Contains => serde_json::json!({ "contains": arg }),
            JsonOperator::Length => serde_json::json!({ "length": arg }),
            JsonOperator::Matches => serde_json::json!({ "matches": arg }),
        }
    }

    /// The exact reverse of [`Self::wrap`] — a best-effort *display* guess at
    /// which operator an already-parsed `json:` value was written with, so
    /// `EditState::new` can preload the edit UI from a real request. Mirrors
    /// (but does not call — that function is private to sendra-core) the
    /// same single-key-object shape `JsonSpec::parse` recognises; whichever
    /// operator this guesses, `Self::wrap` is what actually gets saved, and
    /// `Assertions::evaluate` — never this function — is what decides what
    /// the saved value means once the request runs.
    pub(super) fn detect(value: &serde_json::Value) -> (Self, serde_json::Value) {
        if let serde_json::Value::Object(map) = value {
            if map.len() == 1 {
                for (operator, key) in [
                    (JsonOperator::GreaterThan, "greater_than"),
                    (JsonOperator::GreaterThanOrEqual, "greater_than_or_equal"),
                    (JsonOperator::LessThan, "less_than"),
                    (JsonOperator::LessThanOrEqual, "less_than_or_equal"),
                    (JsonOperator::Contains, "contains"),
                    (JsonOperator::Length, "length"),
                    (JsonOperator::Matches, "matches"),
                ] {
                    if let Some(arg) = map.get(key) {
                        return (operator, arg.clone());
                    }
                }
            }
        }
        (JsonOperator::Equals, value.clone())
    }
}

/// One row of a `json:` path assertion: a JSON path, an operator (see
/// [`JsonOperator`]), the operator's argument as typed, and whether this row
/// is asserted plain or negated. Mirrors [`HeaderRow`]'s role for
/// `Request::headers` — a working copy of one entry in a real map, seeded
/// from it and converted back to it — except the destination is
/// `Assertions::json`/`NotAssertions::json` rather than a single map, since
/// a row's `negate` flag decides which of the two it belongs in (see
/// `EditState::to_assertions`).
///
/// **Scoping decision for this issue.** `sendra_core::Assertions` is not one
/// homogeneous list — it is seven different kinds of check (`status`,
/// `status_in`, `headers`, `body_contains`, `body_matches`,
/// `elapsed_ms_under`, `json`), each its own field, plus a `not:` wrapper
/// duplicating the same seven for negation. Only `json:` (JSON-path +
/// operator + expected value) is genuinely list-shaped the way
/// `Request::headers` is — the other six are singular optional values, each
/// of which would need its own single-field editor in the shape of
/// `AuthEdit`'s Bearer/Basic fields, and `not:` would double every one of
/// them again. This issue's row-based add/edit/delete UI covers `json:`
/// (and, through each row's `negate` flag, `not: {json: {...}}`) end to
/// end; `status`/`status_in`/`headers`/`body_contains`/`body_matches`/
/// `elapsed_ms_under`, and every other kind under `not:`, are left
/// completely untouched by this editor (see `EditState::to_assertions`,
/// which round-trips them from the original request verbatim) — an honest
/// gap to revisit as its own issue, not a silent one: nothing here claims to
/// offer editing for them, the same "say what isn't covered, in the pane
/// itself" bar `BodyEdit`'s `body_file`/`form`/`multipart` note and
/// `AuthEdit`'s OAuth note already set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssertionRow {
    pub path: TextField,
    pub operator: JsonOperator,
    pub value: TextField,
    pub negate: bool,
    /// `Some(message)` whenever `value`'s current text does not parse as
    /// YAML (and so has no `serde_json::Value` equivalent) — recomputed on
    /// every keystroke that touches `value`, the same live-validation
    /// `method_error` already gets (see `update::edit_mutate`). This is the
    /// same thing `Assertions::json`'s own `Deserialize` would reject a file
    /// for — a value with no JSON equivalent is a parse error there too, not
    /// a new, stricter rule invented here — so `Message::SaveEdit` refuses
    /// to save while any row has this set, the same way it refuses over a
    /// bad `method`. Unlike a `json:` value's *operator-argument* type (a
    /// `greater_than` against a string, say), which sendra-core only checks
    /// once the response actually arrives and so is never rejected here —
    /// see `AssertionRow::to_json_value`.
    pub value_error: Option<String>,
}

impl AssertionRow {
    /// Visible to `EditState::new`, which seeds one of these per entry in
    /// the real request's `assertions.json`/`assertions.not.json`.
    /// `negate` says which of the two `value` came from — `false` for
    /// `assertions.json`, `true` for `assertions.not.json`.
    fn from_existing(path: &str, value: &serde_json::Value, negate: bool) -> Self {
        let (operator, arg) = JsonOperator::detect(value);
        let text = match &arg {
            // Displayed without the surrounding quotes a `to_string` would
            // add, so re-editing a string value shows the same plain text a
            // person would have typed in the file (`ada`, not `"ada"`) —
            // still exactly what `to_json_value` parses back out, since bare
            // YAML text is a string.
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        Self {
            path: TextField::new(path),
            operator,
            value: TextField::new(text),
            negate,
            // Built from a value that already parsed successfully out of a
            // real, loaded request — there is nothing to reject.
            value_error: None,
        }
    }

    /// This row's `value`, parsed as YAML (the same JSON-value grammar
    /// `Assertions::json`'s own `Deserialize` holds every expected value to
    /// — see `Self::value_error`), wrapped in whichever operator shape
    /// [`JsonOperator::wrap`] builds. `Err` is exactly
    /// [`Self::value_error`]'s own message; recomputing it here rather than
    /// trusting a possibly-stale field is what lets `EditState::to_assertions`
    /// call this directly without also having to check `value_error` first.
    fn to_json_value(&self) -> Result<serde_json::Value, String> {
        let arg = validate_assertion_value_text(self.value.value())?;
        Ok(self.operator.wrap(arg))
    }
}

/// Whether `text` parses as YAML — and so has a `serde_json::Value`
/// equivalent — the same grammar `Assertions::json`'s own `Deserialize`
/// holds every expected value to when a request file is loaded (see the
/// module docs on `sendra_core::assertions`). Reused here, not
/// reimplemented more strictly or more loosely, so a value this UI accepts
/// is exactly one a hand-written YAML file would also have accepted.
///
/// Visible to `super::update`, which calls this on every keystroke that
/// touches an assertion row's value field and again at save time — the same
/// two call sites `validate_method_text` already has.
pub(super) fn validate_assertion_value_text(text: &str) -> Result<serde_json::Value, String> {
    serde_yaml::from_str::<serde_json::Value>(text)
        .map_err(|err| format!("'{text}' is not valid YAML: {err}"))
}

/// Which of `sendra_core::CaptureSource`'s three forms one capture row
/// currently reads from — a real, closed vocabulary, so (like
/// `ApiKeyLocation`/`JsonOperator` before it) this is cycled through
/// (`Message::EditCursorLeft`/`EditCursorRight`), never typed as free text.
/// See `sendra_core::capture`'s module docs for exactly what each one
/// captures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CaptureKind {
    #[default]
    JsonPath,
    Header,
    Status,
}

impl CaptureKind {
    const ALL: [CaptureKind; 3] = [
        CaptureKind::JsonPath,
        CaptureKind::Header,
        CaptureKind::Status,
    ];

    /// Visible to `EditState::toggle_focused`, which calls this for `Right`
    /// and [`Self::prev`] for `Left` — an ordinary forward/backward cycle
    /// through [`Self::ALL`], wrapping at either end, the same shape
    /// `JsonOperator::next` already has.
    pub(super) fn next(self) -> Self {
        cycle_next(self, &Self::ALL)
    }

    pub(super) fn prev(self) -> Self {
        cycle_prev(self, &Self::ALL)
    }

    /// The label shown in the edit pane — `"json path"`, `"header"` or
    /// `"status"` — next to the `(←/→)` toggle hint, the same convention
    /// `JsonOperator::label`/`api_key_location_str` already use.
    pub(super) fn label(self) -> &'static str {
        match self {
            CaptureKind::JsonPath => "json path",
            CaptureKind::Header => "header",
            CaptureKind::Status => "status",
        }
    }
}

/// One entry of a `capture:` block, mid-edit: a variable name, which of
/// `CaptureSource`'s three forms it reads from, and (for the two forms that
/// need one) the path or header name it reads. Mirrors `HeaderRow`'s role for
/// `Request::headers`.
///
/// **Unlike `AssertionRow`, this covers `sendra_core::Captures` completely,
/// not partially.** `Captures` is not seven heterogeneous kinds the way
/// `Assertions` is — it is `#[serde(transparent)]` over a single
/// `BTreeMap<String, CaptureSource>` (see that type's own doc comment), and
/// `CaptureSource` itself is a small, closed, three-variant enum. A row's
/// `name` and `kind` plus one `value` field (the JSON path for
/// `CaptureKind::JsonPath`, the header name for `CaptureKind::Header`, unused
/// for `CaptureKind::Status` — `status: true` has no argument to type) is
/// therefore not a scoped-down slice of the schema the way `AssertionRow`'s
/// `json:`-only coverage is; every capture a request file could express is
/// representable as one of these rows, and `EditState::to_captures` replaces
/// `Request::capture` wholesale rather than layering onto a `base` the way
/// `to_assertions` has to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureRow {
    pub name: TextField,
    pub kind: CaptureKind,
    pub value: TextField,
}

impl CaptureRow {
    /// Visible to `EditState::new`, which seeds one of these per entry in
    /// the real request's `capture:` block.
    fn from_existing(name: &str, source: &CaptureSource) -> Self {
        let (kind, value) = match source {
            CaptureSource::JsonPath(path) => (CaptureKind::JsonPath, path.clone()),
            CaptureSource::Header { header } => (CaptureKind::Header, header.clone()),
            CaptureSource::Status { .. } => (CaptureKind::Status, String::new()),
        };
        Self {
            name: TextField::new(name),
            kind,
            value: TextField::new(value),
        }
    }

    /// The real `CaptureSource` this row saves as — the exact reverse of
    /// `from_existing`. `value` is ignored for `CaptureKind::Status`: the
    /// only value `CaptureSource::Status` can ever hold is `status: true`
    /// (see that variant's own doc comment on why `status: false` isn't a
    /// thing this UI could even build).
    fn to_capture_source(&self) -> CaptureSource {
        match self.kind {
            CaptureKind::JsonPath => CaptureSource::JsonPath(self.value.value().to_string()),
            CaptureKind::Header => CaptureSource::Header {
                header: self.value.value().to_string(),
            },
            CaptureKind::Status => CaptureSource::Status { status: true },
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
    /// Working copy of `Request::name`. Empty means "no name" — see
    /// `EditState::to_request`/`apply_edit_to_request` in `update`, which
    /// save it back through the same `non_empty` convention `AuthEdit`'s
    /// optional OAuth fields already use, so clearing this field back to
    /// blank saves as the request genuinely having no name again, not a
    /// name that happens to be the empty string. Never itself rejected the
    /// way an invalid `method` is — see `EditField::Name`'s own doc comment
    /// for why "is a name required here" is not a question this type can
    /// answer on its own.
    pub name: TextField,
    pub method: TextField,
    pub url: TextField,
    /// Working copies of the request's headers, in the same order as
    /// `Request::headers` — seeded once by `EditState::new` and written back
    /// wholesale by `Message::SaveEdit`, the same round-trip `method`/`url`
    /// already go through.
    pub headers: Vec<HeaderRow>,
    /// The selected request's body, however it participates in this edit —
    /// see `BodyEdit`'s own doc comment for what is and isn't editable.
    pub body: BodyEdit,
    /// The selected request's `auth:` block, however it participates in this
    /// edit — see `AuthEdit`'s own doc comment for what is and isn't
    /// editable.
    pub auth: AuthEdit,
    /// Working copies of the request's `json:` path assertions (and, via
    /// each row's `negate` flag, its `not: {json: {...}}` ones) — see
    /// `AssertionRow`'s own doc comment for exactly what this does and does
    /// not cover of `sendra_core::Assertions`.
    pub assertions: Vec<AssertionRow>,
    /// Working copies of the request's `capture:` block — see `CaptureRow`'s
    /// own doc comment for why, unlike `assertions`, this covers
    /// `sendra_core::Captures` completely rather than a scoped-down slice of
    /// it.
    pub captures: Vec<CaptureRow>,
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
    /// `Some(message)` whenever the body is a `json:`-mode `BodyEdit::Editable`
    /// whose current text does not parse as JSON. Unlike `method_error`,
    /// this is deliberately **not** recomputed on every keystroke — a body
    /// mid-edit is expected to pass through many syntactically-invalid
    /// states between meaningful ones (an open brace with nothing after it
    /// yet, say), and flagging every one of them would make ordinary typing
    /// look like a wall of errors. Instead `Message::SaveEdit` computes this
    /// fresh only when save is actually attempted (see
    /// `update::validate_body_for_save`), and typing anything into the body
    /// field after a failed save clears the stale message immediately (see
    /// `update::edit_mutate`) rather than leaving a now-possibly-wrong error
    /// on screen until the next save attempt.
    pub body_error: Option<String>,
    /// `Some(message)` right after `Message::SaveEdit` attempted to write the
    /// edit to disk (via `sendra_core::Document::save_to_path`) and that
    /// write failed — a full disk, a permissions error, a removed directory,
    /// anything `save_to_path`'s own doc comment covers. Unlike
    /// `method_error`/`body_error`, this is never something the user can fix
    /// by typing — it is an environment problem, not a bad value — so
    /// `edit_mutate` never touches it; it clears only when a save attempt
    /// actually succeeds (`Message::SaveEdit`'s own arm) or the whole edit is
    /// cancelled (dropped along with the rest of `EditState`). Reusing
    /// `format_error`'s inline rendering (see `render_edit_pane`) is what
    /// "issue 11's render_error/format_error path" means here: the same
    /// `⚠ heading\nerror` shape a failed run or a failed collection load
    /// already use, not a new error-display convention just for this.
    pub save_error: Option<String>,
}

/// Appends `T::default()` onto `rows` and returns the `EditField` that
/// should receive focus afterward (`first_field` applied to the new row's
/// index) — the shared shell behind `EditState::add_header_row`/
/// `add_assertion_row`/`add_capture_row`, which differ only in which `Vec`
/// and which `EditField` variant wraps a row index.
fn add_row<T: Default>(
    rows: &mut Vec<T>,
    first_field: impl FnOnce(usize) -> EditField,
) -> EditField {
    rows.push(T::default());
    first_field(rows.len() - 1)
}

/// Removes `rows[index]` and returns the `EditField` that should receive
/// focus afterward: the row now at the same position (clamped, via
/// `first_field`), or `fallback()` if that was the last row — the shared
/// shell behind `EditState::delete_focused_header_row`/
/// `delete_focused_assertion_row`/`delete_focused_capture_row`, which differ
/// only in which `Vec`, which `EditField` variant wraps a row index, and
/// what precedes the section once it empties out. Callers are responsible
/// for first checking that `focus` actually names a row in `rows` at all —
/// see each caller's own exhaustive match on `EditField`.
fn delete_row<T>(
    rows: &mut Vec<T>,
    index: usize,
    first_field: impl FnOnce(usize) -> EditField,
    fallback: impl FnOnce() -> EditField,
) -> EditField {
    rows.remove(index);
    if rows.is_empty() {
        fallback()
    } else {
        first_field(index.saturating_sub(1).min(rows.len() - 1))
    }
}

impl EditState {
    /// Visible to `super::update`'s `Message::EnterEditMode` arm, which is
    /// the only place outside this module allowed to start an edit.
    pub(super) fn new(request: &Request) -> Self {
        let name = TextField::new(request.name.clone().unwrap_or_default());
        let method = TextField::new(request.method.as_str());
        let method_error = validate_method_text(method.value()).err();
        let headers = request
            .headers
            .iter()
            .map(|(name, value)| HeaderRow::new(name.clone(), value.clone()))
            .collect();
        let assertions = request
            .assertions
            .as_ref()
            .map(|assertions| {
                // `json:` rows first, in path order (a `BTreeMap`'s own
                // iteration order), then `not: {json: {...}}` rows the same
                // way — a fixed, deterministic order rather than one that
                // depends on how the map happened to be built.
                let mut rows: Vec<AssertionRow> = assertions
                    .json
                    .iter()
                    .map(|(path, value)| AssertionRow::from_existing(path, value, false))
                    .collect();
                if let Some(not) = &assertions.not {
                    rows.extend(
                        not.json
                            .iter()
                            .map(|(path, value)| AssertionRow::from_existing(path, value, true)),
                    );
                }
                rows
            })
            .unwrap_or_default();
        let captures = request
            .capture
            .as_ref()
            .map(|captures| {
                captures
                    .entries()
                    .iter()
                    .map(|(name, source)| CaptureRow::from_existing(name, source))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            dirty: false,
            name,
            method,
            url: TextField::new(request.url.clone()),
            headers,
            body: BodyEdit::new(request),
            auth: AuthEdit::new(request.auth.as_ref()),
            assertions,
            captures,
            focus: EditField::default(),
            method_error,
            body_error: None,
            save_error: None,
        }
    }

    /// The focused field's `TextField`, or `None` when focus is on a
    /// fixed-enum auth sub-field with no `TextField` behind it at all (see
    /// `AuthEdit::text_field_mut`). Visible to `super::update`'s
    /// `edit_mutate`/`edit_move`/`edit_move_or_toggle` helpers.
    pub(super) fn focused_field_mut(&mut self) -> Option<&mut TextField> {
        match self.focus {
            EditField::Name => Some(&mut self.name),
            EditField::Method => Some(&mut self.method),
            EditField::Url => Some(&mut self.url),
            EditField::HeaderKey(index) => Some(&mut self.headers[index].key),
            EditField::HeaderValue(index) => Some(&mut self.headers[index].value),
            EditField::Body => match &mut self.body {
                BodyEdit::Editable { text, .. } => Some(text),
                BodyEdit::Unsupported { .. } => unreachable!(
                    "focus is never Body while the body is Unsupported — see \
                     EditField::next/prev, which only ever move focus onto Body \
                     when has_body is true"
                ),
            },
            EditField::Auth(field) => self.auth.text_field_mut(field),
            EditField::AssertionPath(index) => Some(&mut self.assertions[index].path),
            EditField::AssertionValue(index) => Some(&mut self.assertions[index].value),
            EditField::AssertionOperator(_) | EditField::AssertionNegate(_) => None,
            EditField::CaptureName(index) => Some(&mut self.captures[index].name),
            EditField::CaptureValue(index) => Some(&mut self.captures[index].value),
            EditField::CaptureKind(_) => None,
        }
    }

    /// Appends a new, empty header row at the end and moves focus straight
    /// to its key field — visible to `super::update`'s `Message::AddHeaderRow`
    /// arm.
    pub(super) fn add_header_row(&mut self) {
        self.focus = add_row(&mut self.headers, EditField::HeaderKey);
    }

    /// Removes whichever header row `focus` currently points at — a no-op
    /// when focus is not on a header row at all (there is nothing to
    /// delete). Focus afterward never dangles on a removed row: it moves to
    /// the previous row's key (or, if the deleted row was the first one,
    /// the new first row's key), or to `Url` if no header rows remain.
    /// Visible to `super::update`'s `Message::DeleteHeaderRow` arm.
    pub(super) fn delete_focused_header_row(&mut self) {
        let index = match self.focus {
            EditField::HeaderKey(index) | EditField::HeaderValue(index) => index,
            EditField::Name
            | EditField::Method
            | EditField::Url
            | EditField::Body
            | EditField::Auth(_)
            | EditField::AssertionPath(_)
            | EditField::AssertionOperator(_)
            | EditField::AssertionValue(_)
            | EditField::AssertionNegate(_)
            | EditField::CaptureName(_)
            | EditField::CaptureKind(_)
            | EditField::CaptureValue(_) => return,
        };
        self.focus = delete_row(&mut self.headers, index, EditField::HeaderKey, || {
            EditField::Url
        });
    }

    /// Appends a new, empty assertion row at the end and moves focus
    /// straight to its path field — visible to `super::update`'s
    /// `Message::AddAssertionRow` arm. Shares [`add_row`] with
    /// `add_header_row`/`add_capture_row`.
    pub(super) fn add_assertion_row(&mut self) {
        self.focus = add_row(&mut self.assertions, EditField::AssertionPath);
    }

    /// Removes whichever assertion row `focus` currently points at — a
    /// no-op when focus is not on an assertion row at all. Focus afterward
    /// never dangles on a removed row: it moves to the previous row's path
    /// (or, if the deleted row was the first one, the new first row's
    /// path), or to whatever field would ordinarily precede the assertions
    /// section if none remain (see `EditField::before_assertions`). Shares
    /// [`delete_row`] with `delete_focused_header_row`/
    /// `delete_focused_capture_row`. Visible to `super::update`'s
    /// `Message::DeleteAssertionRow` arm.
    pub(super) fn delete_focused_assertion_row(&mut self) {
        let index = match self.focus {
            EditField::AssertionPath(index)
            | EditField::AssertionOperator(index)
            | EditField::AssertionValue(index)
            | EditField::AssertionNegate(index) => index,
            EditField::Name
            | EditField::Method
            | EditField::Url
            | EditField::HeaderKey(_)
            | EditField::HeaderValue(_)
            | EditField::Body
            | EditField::Auth(_)
            | EditField::CaptureName(_)
            | EditField::CaptureKind(_)
            | EditField::CaptureValue(_) => return,
        };
        let headers_len = self.headers.len();
        let has_editable_body = self.has_editable_body();
        let auth_field_order = self.auth_field_order();
        self.focus = delete_row(
            &mut self.assertions,
            index,
            EditField::AssertionPath,
            || EditField::before_assertions(headers_len, has_editable_body, auth_field_order),
        );
    }

    /// Appends a new, empty capture row at the end and moves focus straight
    /// to its name field — visible to `super::update`'s
    /// `Message::AddCaptureRow` arm. Shares [`add_row`] with
    /// `add_header_row`/`add_assertion_row`.
    pub(super) fn add_capture_row(&mut self) {
        self.focus = add_row(&mut self.captures, EditField::CaptureName);
    }

    /// Removes whichever capture row `focus` currently points at — a no-op
    /// when focus is not on a capture row at all. Focus afterward never
    /// dangles on a removed row: it moves to the previous row's name (or, if
    /// the deleted row was the first one, the new first row's name), or to
    /// whatever field would ordinarily precede the captures section if none
    /// remain (see `EditField::before_captures`). Shares [`delete_row`] with
    /// `delete_focused_header_row`/`delete_focused_assertion_row`. Visible
    /// to `super::update`'s `Message::DeleteCaptureRow` arm.
    pub(super) fn delete_focused_capture_row(&mut self) {
        let index = match self.focus {
            EditField::CaptureName(index)
            | EditField::CaptureKind(index)
            | EditField::CaptureValue(index) => index,
            EditField::Name
            | EditField::Method
            | EditField::Url
            | EditField::HeaderKey(_)
            | EditField::HeaderValue(_)
            | EditField::Body
            | EditField::Auth(_)
            | EditField::AssertionPath(_)
            | EditField::AssertionOperator(_)
            | EditField::AssertionValue(_)
            | EditField::AssertionNegate(_) => return,
        };
        let headers_len = self.headers.len();
        let has_editable_body = self.has_editable_body();
        let auth_field_order = self.auth_field_order();
        let assertion_row_count = self.assertion_row_count();
        self.focus = delete_row(&mut self.captures, index, EditField::CaptureName, || {
            EditField::before_captures(
                headers_len,
                has_editable_body,
                auth_field_order,
                assertion_row_count,
            )
        });
    }

    /// Whether `body` currently has a real text field to focus at all — what
    /// `EditField::next`/`prev` need to decide whether `Body` belongs in the
    /// focus cycle right now. `true` for `BodyEdit::Editable`, `false` for
    /// `BodyEdit::Unsupported`.
    pub(super) fn has_editable_body(&self) -> bool {
        matches!(self.body, BodyEdit::Editable { .. })
    }

    /// Which `AuthField`s `EditField::next`/`prev` should cycle through right
    /// now — empty for a request with no `auth:` at all. Visible to
    /// `super::update`'s `Message::EditFocusNext`/`EditFocusPrev` arms and to
    /// `render_edit_pane`.
    pub(super) fn auth_field_order(&self) -> &'static [AuthField] {
        self.auth.field_order()
    }

    /// How many assertion rows `EditField::next`/`prev` should cycle
    /// through right now — visible to `super::update`'s
    /// `Message::EditFocusNext`/`EditFocusPrev` arms and to
    /// `render_edit_pane`.
    pub(super) fn assertion_row_count(&self) -> usize {
        self.assertions.len()
    }

    /// How many capture rows `EditField::next`/`prev` should cycle through
    /// right now — visible to `super::update`'s
    /// `Message::EditFocusNext`/`EditFocusPrev` arms and to
    /// `render_edit_pane`.
    pub(super) fn capture_row_count(&self) -> usize {
        self.captures.len()
    }

    /// Toggles whichever fixed-enum sub-field focus currently points at —
    /// an auth sub-field (`api_key.in`/`oauth.grant_type`, via
    /// `AuthEdit::toggle`, always a plain flip regardless of `forward`), an
    /// assertion row's `operator` (cycled forward/backward through
    /// [`JsonOperator::next`]/[`JsonOperator::prev`]) or `negate` (a plain
    /// flip, like the auth sub-fields), or a capture row's `kind` (cycled
    /// forward/backward through [`CaptureKind::next`]/[`CaptureKind::prev`]).
    /// A no-op returning `false` whenever focus is on none of these — every
    /// other field is a `TextField` with a real cursor to move instead.
    /// Visible to `super::update::edit_move_or_toggle`, which calls this
    /// before falling back to ordinary cursor movement.
    pub(super) fn toggle_focused(&mut self, forward: bool) -> bool {
        match self.focus {
            EditField::Auth(field) => self.auth.toggle(field),
            EditField::AssertionOperator(index) => {
                let row = &mut self.assertions[index];
                row.operator = if forward {
                    row.operator.next()
                } else {
                    row.operator.prev()
                };
                true
            }
            EditField::AssertionNegate(index) => {
                self.assertions[index].negate = !self.assertions[index].negate;
                true
            }
            EditField::CaptureKind(index) => {
                let row = &mut self.captures[index];
                row.kind = if forward {
                    row.kind.next()
                } else {
                    row.kind.prev()
                };
                true
            }
            _ => false,
        }
    }

    /// Builds a full candidate `Request` reflecting every field this edit
    /// session might change (`method`/`url`/`headers`/`auth`/`assertions`),
    /// layered onto `base`'s other fields verbatim. This is what
    /// `render_edit_pane`'s live "resolved auth" preview resolves against,
    /// via the exact same `Environment::apply`/`Request::resolve_auth`
    /// pipeline `Message::SaveEdit` and the read-only (non-editing) preview
    /// already use — so a mid-edit preview of "what auth would actually be
    /// sent" is provably correct, reusing sendra-core's own resolution,
    /// rather than a second, hand-rolled formatting of `AuthEdit`. An
    /// invalid `method` leaves `base`'s own method in the candidate, the
    /// same "don't guess" rule `Message::SaveEdit` already follows for a
    /// method it refuses to save.
    pub(super) fn to_request(&self, base: &Request) -> Request {
        let mut request = base.clone();
        request.name = non_empty(self.name.value());
        if let Ok(method) = validate_method_text(self.method.value()) {
            request.method = method;
        }
        request.url = self.url.value().to_string();
        request.headers = self
            .headers
            .iter()
            .map(|row| (row.key.value().to_string(), row.value.value().to_string()))
            .collect();
        request.auth = self.auth.to_auth();
        request.assertions = self.to_assertions(base.assertions.as_ref());
        request.capture = self.to_captures();
        request
    }

    /// Reassembles `self.assertions`'s rows back into a real
    /// `sendra_core::Assertions`, layered onto `base` (the request's
    /// original assertions block, before this edit session) the same way
    /// `to_request` layers `method`/`url`/`headers`/`auth` onto a real
    /// `Request`: every field this editor does not offer —
    /// `status`/`status_in`/`headers`/`body_contains`/`body_matches`/
    /// `elapsed_ms_under`, and the same six again under `not:` — is carried
    /// over from `base` completely untouched (see `AssertionRow`'s own doc
    /// comment for why those are out of scope here). Only `json` and
    /// `not.json` are replaced, built fresh from the rows: a row whose
    /// `negate` is `false` goes into `json`, `true` goes into `not.json`,
    /// keyed by its `path` — a later row with the same path as an earlier
    /// one overwrites it, the same last-one-wins rule a hand-written YAML
    /// file with a duplicate mapping key would already get from `serde_yaml`.
    /// A row whose `value` fails to parse (see `AssertionRow::value_error`)
    /// is skipped rather than saved malformed; `Message::SaveEdit` already
    /// refuses to reach this at all while any row has one, so this is a
    /// last-resort guard against a stale invariant, not the primary check.
    ///
    /// Returns `None` when the result would assert nothing at all — an
    /// edit session that deleted every row from a request that had no other
    /// assertion kind set either must not leave behind an empty, pointless
    /// `assertions: {}` block.
    pub(super) fn to_assertions(&self, base: Option<&Assertions>) -> Option<Assertions> {
        let mut assertions = base.cloned().unwrap_or_default();
        let mut positive = BTreeMap::new();
        let mut negated = BTreeMap::new();
        for row in &self.assertions {
            if let Ok(value) = row.to_json_value() {
                let path = row.path.value().to_string();
                if row.negate {
                    negated.insert(path, value);
                } else {
                    positive.insert(path, value);
                }
            }
        }
        assertions.json = positive;
        match &mut assertions.not {
            Some(not) => not.json = negated,
            None if !negated.is_empty() => {
                assertions.not = Some(NotAssertions {
                    json: negated,
                    ..NotAssertions::default()
                });
            }
            None => {}
        }
        if assertions.is_empty() {
            None
        } else {
            Some(assertions)
        }
    }

    /// Reassembles `self.captures`'s rows into a real `sendra_core::Captures`
    /// — unlike `to_assertions`, this takes no `base` to layer onto:
    /// `Captures` is `entries` and nothing else (see `CaptureRow`'s own doc
    /// comment), so every row this edit session holds is the *entire*
    /// `capture:` block, not a slice of it. A later row with the same `name`
    /// as an earlier one overwrites it, the same last-one-wins rule
    /// `to_assertions` already follows for a duplicate path.
    ///
    /// Returns `None` when the result would capture nothing at all — an edit
    /// session that deleted every row must not leave behind an empty,
    /// pointless `capture: {}` block.
    pub(super) fn to_captures(&self) -> Option<Captures> {
        let captures: Captures = self
            .captures
            .iter()
            .map(|row| (row.name.value().to_string(), row.to_capture_source()))
            .collect();
        if captures.is_empty() {
            None
        } else {
            Some(captures)
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
/// request. Chosen generously for what browsing actually needs — nobody
/// scrolls back through more than a handful of past runs while debugging a
/// single request — while still bounding the real memory cost this issue was
/// asked to consider: nothing in sendra-core caps a response body's size, and
/// a long session that re-sends the same request many times would otherwise
/// let those bodies accumulate in memory without limit. Once a request's
/// history grows past this, its oldest entry is dropped to make room for the
/// newest — see `update::push_history_entry`.
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

    // --- TextField: multi-line (the body editor) ------------------------------

    #[test]
    fn insert_char_with_a_newline_creates_a_second_line() {
        let mut field = TextField::new("ab");
        field.move_left(); // cursor between 'a' and 'b'

        field.insert_char('\n');

        assert_eq!(field.value(), "a\nb");
        assert_eq!(
            field.cursor_row_col(),
            (1, 0),
            "the cursor must now be at the very start of the new second line"
        );
    }

    #[test]
    fn backspace_right_after_a_newline_merges_the_two_lines() {
        let mut field = TextField::new("one\ntwo");
        field.move_to_row_col(1, 0); // start of "two"

        field.backspace();

        assert_eq!(field.value(), "onetwo");
        assert_eq!(field.cursor_row_col(), (0, 3));
    }

    #[test]
    fn delete_right_before_a_newline_merges_the_two_lines() {
        let mut field = TextField::new("one\ntwo");
        field.move_to_row_col(0, 3); // end of "one", right before '\n'

        field.delete();

        assert_eq!(field.value(), "onetwo");
        assert_eq!(field.cursor_row_col(), (0, 3));
    }

    #[test]
    fn cursor_row_col_counts_preceding_newlines_and_the_column_within_the_current_line() {
        let mut field = TextField::new("ab\ncde\nf");
        field.move_to_row_col(2, 1); // "f" is the whole third line

        assert_eq!(field.cursor_row_col(), (2, 1));
    }

    #[test]
    fn move_up_keeps_the_same_column_when_the_line_above_is_at_least_as_wide() {
        let mut field = TextField::new("abcd\nxy");
        field.move_to_row_col(1, 2); // end of "xy"

        field.move_up();

        assert_eq!(field.cursor_row_col(), (0, 2));
    }

    #[test]
    fn move_up_clamps_to_the_end_of_a_shorter_line_above() {
        let mut field = TextField::new("ab\nwxyz");
        field.move_to_row_col(1, 4); // end of "wxyz"

        field.move_up();

        assert_eq!(
            field.cursor_row_col(),
            (0, 2),
            "the line above is only 2 chars wide, so the cursor clamps to its end"
        );
    }

    #[test]
    fn move_up_on_the_first_line_is_a_no_op() {
        let mut field = TextField::new("abc");
        field.move_to_row_col(0, 1);

        field.move_up();

        assert_eq!(field.cursor_row_col(), (0, 1));
    }

    #[test]
    fn move_down_keeps_the_same_column_when_the_line_below_is_at_least_as_wide() {
        let mut field = TextField::new("xy\nabcd");
        field.move_to_row_col(0, 2);

        field.move_down();

        assert_eq!(field.cursor_row_col(), (1, 2));
    }

    #[test]
    fn move_down_clamps_to_the_end_of_a_shorter_line_below() {
        let mut field = TextField::new("wxyz\nab");
        field.move_to_row_col(0, 4);

        field.move_down();

        assert_eq!(field.cursor_row_col(), (1, 2));
    }

    #[test]
    fn move_down_on_the_last_line_is_a_no_op() {
        let mut field = TextField::new("abc");
        field.move_to_row_col(0, 1);

        field.move_down();

        assert_eq!(field.cursor_row_col(), (0, 1));
    }

    #[test]
    fn move_up_then_down_returns_to_the_original_column() {
        let mut field = TextField::new("hello\nworld");
        field.move_to_row_col(1, 3);

        field.move_up();
        field.move_down();

        assert_eq!(field.cursor_row_col(), (1, 3));
    }

    #[test]
    fn edit_field_next_walks_name_method_and_url_in_order_with_nothing_else() {
        assert_eq!(EditField::Name.next(0, false, &[], 0, 0), EditField::Method);
        assert_eq!(EditField::Method.next(0, false, &[], 0, 0), EditField::Url);
        assert_eq!(
            EditField::Url.next(0, false, &[], 0, 0),
            EditField::Name,
            "with nothing else configured, Url wraps back to Name"
        );
    }

    #[test]
    fn edit_field_next_walks_through_header_rows_in_order() {
        assert_eq!(
            EditField::Url.next(2, false, &[], 0, 0),
            EditField::HeaderKey(0)
        );
        assert_eq!(
            EditField::HeaderKey(0).next(2, false, &[], 0, 0),
            EditField::HeaderValue(0)
        );
        assert_eq!(
            EditField::HeaderValue(0).next(2, false, &[], 0, 0),
            EditField::HeaderKey(1)
        );
        assert_eq!(
            EditField::HeaderKey(1).next(2, false, &[], 0, 0),
            EditField::HeaderValue(1)
        );
        assert_eq!(
            EditField::HeaderValue(1).next(2, false, &[], 0, 0),
            EditField::Name,
            "with no body, auth or assertion field, the last header row's value wraps back to Name"
        );
    }

    #[test]
    fn edit_field_next_visits_body_before_auth_before_assertions() {
        assert_eq!(EditField::Url.next(0, true, &[], 0, 0), EditField::Body);
        assert_eq!(
            EditField::HeaderValue(1).next(2, true, &[], 0, 0),
            EditField::Body,
            "the last header row's value must move into Body when the body is editable"
        );
        assert_eq!(
            EditField::Body.next(2, true, &[], 0, 0),
            EditField::Name,
            "Body wraps back to Name when there is no auth or assertion field"
        );
        assert_eq!(
            EditField::Body.next(2, true, &[AuthField::BearerToken], 0, 0),
            EditField::Auth(AuthField::BearerToken),
            "Body moves into Auth when there is one"
        );
        assert_eq!(
            EditField::Body.next(2, true, &[], 3, 0),
            EditField::AssertionPath(0),
            "Body moves into the first assertion row when there is no auth but there are \
             assertion rows"
        );
    }

    #[test]
    fn edit_field_next_skips_body_entirely_when_unsupported() {
        assert_eq!(
            EditField::Url.next(0, false, &[], 0, 0),
            EditField::Name,
            "with no headers, no editable body, no auth and no assertions, Url wraps straight \
             back to Name"
        );
        assert_eq!(
            EditField::HeaderValue(1).next(2, false, &[], 0, 0),
            EditField::Name
        );
    }

    #[test]
    fn edit_field_next_walks_through_auth_fields_then_into_assertions() {
        let auth_fields = [
            AuthField::ApiKeyName,
            AuthField::ApiKeyValue,
            AuthField::ApiKeyLocation,
        ];
        assert_eq!(
            EditField::Url.next(0, false, &auth_fields, 0, 0),
            EditField::Auth(AuthField::ApiKeyName),
            "with no headers/body, Url moves straight into the first auth field"
        );
        assert_eq!(
            EditField::Auth(AuthField::ApiKeyName).next(0, false, &auth_fields, 0, 0),
            EditField::Auth(AuthField::ApiKeyValue)
        );
        assert_eq!(
            EditField::Auth(AuthField::ApiKeyValue).next(0, false, &auth_fields, 0, 0),
            EditField::Auth(AuthField::ApiKeyLocation)
        );
        assert_eq!(
            EditField::Auth(AuthField::ApiKeyLocation).next(0, false, &auth_fields, 0, 0),
            EditField::Name,
            "the last auth field wraps back to Name when there are no assertion rows"
        );
        assert_eq!(
            EditField::Auth(AuthField::ApiKeyLocation).next(0, false, &auth_fields, 2, 0),
            EditField::AssertionPath(0),
            "the last auth field moves into the first assertion row when there are any"
        );
    }

    #[test]
    fn edit_field_next_walks_through_one_assertion_rows_four_sub_fields_and_wraps_to_name() {
        assert_eq!(
            EditField::AssertionPath(0).next(0, false, &[], 1, 0),
            EditField::AssertionOperator(0)
        );
        assert_eq!(
            EditField::AssertionOperator(0).next(0, false, &[], 1, 0),
            EditField::AssertionValue(0)
        );
        assert_eq!(
            EditField::AssertionValue(0).next(0, false, &[], 1, 0),
            EditField::AssertionNegate(0)
        );
        assert_eq!(
            EditField::AssertionNegate(0).next(0, false, &[], 1, 0),
            EditField::Name,
            "the only assertion row's negate flag wraps back to Name"
        );
    }

    #[test]
    fn edit_field_next_moves_from_one_assertion_row_into_the_next() {
        assert_eq!(
            EditField::AssertionNegate(0).next(0, false, &[], 2, 0),
            EditField::AssertionPath(1),
            "the first row's negate flag moves into the second row's path"
        );
        assert_eq!(
            EditField::AssertionNegate(1).next(0, false, &[], 2, 0),
            EditField::Name,
            "the last row's negate flag wraps back to Name"
        );
    }

    #[test]
    fn edit_field_next_and_prev_are_exact_inverses_across_every_layout() {
        let auth_layouts: [&[AuthField]; 4] = [
            &[],
            &[AuthField::BearerToken],
            &[AuthField::BasicUser, AuthField::BasicPass],
            &[
                AuthField::ApiKeyName,
                AuthField::ApiKeyValue,
                AuthField::ApiKeyLocation,
            ],
        ];
        for header_count in [0, 1, 3] {
            for has_body in [false, true] {
                for auth_fields in auth_layouts {
                    for assertion_row_count in [0, 1, 2] {
                        for capture_row_count in [0, 1, 2] {
                            let mut every_field =
                                vec![EditField::Name, EditField::Method, EditField::Url];
                            for index in 0..header_count {
                                every_field.push(EditField::HeaderKey(index));
                                every_field.push(EditField::HeaderValue(index));
                            }
                            if has_body {
                                every_field.push(EditField::Body);
                            }
                            for &field in auth_fields {
                                every_field.push(EditField::Auth(field));
                            }
                            for index in 0..assertion_row_count {
                                every_field.push(EditField::AssertionPath(index));
                                every_field.push(EditField::AssertionOperator(index));
                                every_field.push(EditField::AssertionValue(index));
                                every_field.push(EditField::AssertionNegate(index));
                            }
                            for index in 0..capture_row_count {
                                every_field.push(EditField::CaptureName(index));
                                every_field.push(EditField::CaptureKind(index));
                                every_field.push(EditField::CaptureValue(index));
                            }
                            for field in every_field {
                                assert_eq!(
                                    field
                                        .next(
                                            header_count,
                                            has_body,
                                            auth_fields,
                                            assertion_row_count,
                                            capture_row_count,
                                        )
                                        .prev(
                                            header_count,
                                            has_body,
                                            auth_fields,
                                            assertion_row_count,
                                            capture_row_count,
                                        ),
                                    field,
                                    "prev must exactly undo next for {field:?} \
                                     (header_count={header_count}, has_body={has_body}, \
                                     auth_fields={auth_fields:?}, \
                                     assertion_row_count={assertion_row_count}, \
                                     capture_row_count={capture_row_count})"
                                );
                                assert_eq!(
                                    field
                                        .prev(
                                            header_count,
                                            has_body,
                                            auth_fields,
                                            assertion_row_count,
                                            capture_row_count,
                                        )
                                        .next(
                                            header_count,
                                            has_body,
                                            auth_fields,
                                            assertion_row_count,
                                            capture_row_count,
                                        ),
                                    field,
                                    "next must exactly undo prev for {field:?} \
                                     (header_count={header_count}, has_body={has_body}, \
                                     auth_fields={auth_fields:?}, \
                                     assertion_row_count={assertion_row_count}, \
                                     capture_row_count={capture_row_count})"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn edit_field_prev_from_method_is_always_name() {
        assert_eq!(EditField::Method.prev(0, false, &[], 0, 0), EditField::Name);
        assert_eq!(
            EditField::Method.prev(2, true, &[AuthField::BearerToken], 2, 3),
            EditField::Name
        );
    }

    #[test]
    fn edit_field_prev_from_name_prefers_assertions_over_auth_body_and_headers() {
        assert_eq!(
            EditField::Name.prev(2, true, &[AuthField::BearerToken], 2, 0),
            EditField::AssertionNegate(1)
        );
    }

    #[test]
    fn edit_field_prev_from_name_prefers_captures_over_assertions_auth_body_and_headers() {
        assert_eq!(
            EditField::Name.prev(2, true, &[AuthField::BearerToken], 2, 3),
            EditField::CaptureValue(2)
        );
    }

    #[test]
    fn edit_field_prev_from_name_prefers_auth_over_body_and_headers_with_no_assertions() {
        assert_eq!(
            EditField::Name.prev(2, true, &[AuthField::BearerToken], 0, 0),
            EditField::Auth(AuthField::BearerToken)
        );
    }

    #[test]
    fn edit_field_prev_from_name_prefers_body_over_headers_with_no_auth_or_assertions() {
        assert_eq!(EditField::Name.prev(2, true, &[], 0, 0), EditField::Body);
    }

    #[test]
    fn edit_field_prev_from_name_wraps_to_the_last_header_value_with_nothing_else() {
        assert_eq!(
            EditField::Name.prev(2, false, &[], 0, 0),
            EditField::HeaderValue(1)
        );
    }

    #[test]
    fn edit_field_prev_from_name_wraps_to_url_with_nothing_at_all() {
        assert_eq!(EditField::Name.prev(0, false, &[], 0, 0), EditField::Url);
    }

    // --- EditState: headers -----------------------------------------------

    fn request_with_headers(headers: &[(&str, &str)]) -> Request {
        let mut yaml = String::from("method: GET\nurl: https://example.com\n");
        if !headers.is_empty() {
            yaml.push_str("headers:\n");
            for (name, value) in headers {
                // Quoted so a value like `*/*` (a YAML alias sigil at the
                // start of a plain scalar) parses as the literal string it
                // is, not as YAML alias syntax.
                yaml.push_str(&format!("  {name}: \"{value}\"\n"));
            }
        }
        Request::from_yaml_str(&yaml).expect("valid test request")
    }

    #[test]
    fn edit_state_new_seeds_header_rows_from_the_real_request_in_order() {
        let request = request_with_headers(&[("Accept", "application/json"), ("X-Env", "prod")]);

        let edit = EditState::new(&request);

        assert_eq!(edit.headers.len(), 2);
        assert_eq!(edit.headers[0].key.value(), "Accept");
        assert_eq!(edit.headers[0].value.value(), "application/json");
        assert_eq!(edit.headers[1].key.value(), "X-Env");
        assert_eq!(edit.headers[1].value.value(), "prod");
    }

    #[test]
    fn edit_state_new_with_no_headers_starts_with_an_empty_list() {
        let request = request_with_headers(&[]);

        let edit = EditState::new(&request);

        assert!(edit.headers.is_empty());
    }

    #[test]
    fn add_header_row_appends_an_empty_row_and_focuses_its_key() {
        let request = request_with_headers(&[("Accept", "*/*")]);
        let mut edit = EditState::new(&request);

        edit.add_header_row();

        assert_eq!(edit.headers.len(), 2);
        assert_eq!(edit.headers[1].key.value(), "");
        assert_eq!(edit.headers[1].value.value(), "");
        assert_eq!(edit.focus, EditField::HeaderKey(1));
    }

    #[test]
    fn delete_focused_header_row_removes_the_focused_row_and_focuses_the_previous_one() {
        let request = request_with_headers(&[("A", "1"), ("B", "2"), ("C", "3")]);
        let mut edit = EditState::new(&request);
        edit.focus = EditField::HeaderValue(1); // "B"

        edit.delete_focused_header_row();

        assert_eq!(edit.headers.len(), 2);
        assert_eq!(edit.headers[0].key.value(), "A");
        assert_eq!(edit.headers[1].key.value(), "C");
        assert_eq!(
            edit.focus,
            EditField::HeaderKey(0),
            "focus must shift to the previous row, not dangle on the removed row"
        );
    }

    #[test]
    fn delete_focused_header_row_deleting_the_first_row_focuses_the_new_first_row() {
        let request = request_with_headers(&[("A", "1"), ("B", "2")]);
        let mut edit = EditState::new(&request);
        edit.focus = EditField::HeaderKey(0); // "A"

        edit.delete_focused_header_row();

        assert_eq!(edit.headers.len(), 1);
        assert_eq!(edit.headers[0].key.value(), "B");
        assert_eq!(edit.focus, EditField::HeaderKey(0));
    }

    #[test]
    fn delete_focused_header_row_with_only_one_row_left_focuses_url() {
        let request = request_with_headers(&[("A", "1")]);
        let mut edit = EditState::new(&request);
        edit.focus = EditField::HeaderKey(0);

        edit.delete_focused_header_row();

        assert!(edit.headers.is_empty());
        assert_eq!(
            edit.focus,
            EditField::Url,
            "focus must not dangle once no header rows remain"
        );
    }

    #[test]
    fn delete_focused_header_row_is_a_no_op_when_focus_is_not_on_a_header_row() {
        let request = request_with_headers(&[("A", "1")]);
        let mut edit = EditState::new(&request);
        edit.focus = EditField::Method;

        edit.delete_focused_header_row();

        assert_eq!(
            edit.headers.len(),
            1,
            "deleting must only ever act on a focused header row"
        );
        assert_eq!(edit.focus, EditField::Method);
    }

    // --- BodyEdit --------------------------------------------------------

    fn request_from(body_yaml: &str) -> Request {
        let yaml = format!("method: POST\nurl: https://example.com\n{body_yaml}");
        Request::from_yaml_str(&yaml).expect("valid test request")
    }

    #[test]
    fn body_edit_new_is_editable_and_empty_for_a_request_with_no_body_at_all() {
        let request = request_from("");

        let body = BodyEdit::new(&request);

        match body {
            BodyEdit::Editable { text, is_json } => {
                assert_eq!(text.value(), "");
                assert!(!is_json, "no body at all defaults to plain, not JSON, mode");
            }
            other => panic!("expected BodyEdit::Editable, got {other:?}"),
        }
    }

    #[test]
    fn body_edit_new_is_editable_plain_for_a_raw_body_field() {
        let request = request_from("body: hello world");

        let body = BodyEdit::new(&request);

        match body {
            BodyEdit::Editable { text, is_json } => {
                assert_eq!(text.value(), "hello world");
                assert!(!is_json);
            }
            other => panic!("expected BodyEdit::Editable, got {other:?}"),
        }
    }

    #[test]
    fn body_edit_new_is_editable_json_and_pretty_printed_for_a_json_body_field() {
        let request = request_from("json:\n  name: ada\n  roles: [admin, user]\n");

        let body = BodyEdit::new(&request);

        match body {
            BodyEdit::Editable { text, is_json } => {
                assert!(is_json);
                // Pretty-printed, not the compact form `serde_yaml` would
                // have produced — real proof it round-trips through
                // `serde_json::Value` and back, not just carried over as a
                // YAML string.
                assert!(
                    text.value().contains("\n"),
                    "expected pretty-printed JSON:\n{}",
                    text.value()
                );
                let reparsed: serde_json::Value =
                    serde_json::from_str(text.value()).expect("must still be valid JSON");
                assert_eq!(reparsed["name"], "ada");
                assert_eq!(reparsed["roles"][0], "admin");
            }
            other => panic!("expected BodyEdit::Editable, got {other:?}"),
        }
    }

    #[test]
    fn body_edit_new_is_unsupported_with_a_visible_path_for_body_file() {
        let request = request_from("body_file: ./payload.json");

        let body = BodyEdit::new(&request);

        match body {
            BodyEdit::Unsupported { description } => {
                assert!(description.contains("./payload.json"));
                assert!(description.contains("not editable"));
            }
            other => panic!("expected BodyEdit::Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn body_edit_new_is_unsupported_for_a_form_body() {
        let request = request_from("form:\n  username: ada\n");

        let body = BodyEdit::new(&request);

        match body {
            BodyEdit::Unsupported { description } => {
                assert!(description.contains("form"));
                assert!(description.contains("not editable"));
            }
            other => panic!("expected BodyEdit::Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn body_edit_new_is_unsupported_for_a_multipart_body() {
        let request = request_from("multipart:\n  - name: description\n    value: hi\n");

        let body = BodyEdit::new(&request);

        match body {
            BodyEdit::Unsupported { description } => {
                assert!(description.contains("multipart"));
                assert!(description.contains("not editable"));
            }
            other => panic!("expected BodyEdit::Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn edit_state_new_reports_has_editable_body_correctly() {
        assert!(EditState::new(&request_from("")).has_editable_body());
        assert!(EditState::new(&request_from("body: x")).has_editable_body());
        assert!(!EditState::new(&request_from("body_file: ./x.json")).has_editable_body());
        assert!(!EditState::new(&request_from("form:\n  a: b\n")).has_editable_body());
    }

    // --- Auth editing ------------------------------------------------------

    fn request_with_auth(auth_yaml: &str) -> Request {
        let yaml = format!("method: GET\nurl: https://example.com\nauth:\n{auth_yaml}");
        Request::from_yaml_str(&yaml).expect("valid test request")
    }

    #[test]
    fn auth_edit_new_is_none_for_a_request_with_no_auth_block() {
        let edit = EditState::new(&request_from(""));
        assert_eq!(edit.auth, AuthEdit::None);
        assert!(edit.auth_field_order().is_empty());
    }

    #[test]
    fn auth_edit_new_seeds_a_bearer_token_from_the_real_request() {
        let request = request_with_auth("  bearer: secret-token\n");

        let edit = EditState::new(&request);

        match &edit.auth {
            AuthEdit::Bearer { token } => assert_eq!(token.value(), "secret-token"),
            other => panic!("expected AuthEdit::Bearer, got {other:?}"),
        }
        assert_eq!(edit.auth_field_order(), &[AuthField::BearerToken]);
    }

    #[test]
    fn auth_edit_new_seeds_basic_user_and_pass_from_the_real_request() {
        let request = request_with_auth("  basic:\n    user: ada\n    pass: hunter2\n");

        let edit = EditState::new(&request);

        match &edit.auth {
            AuthEdit::Basic { user, pass } => {
                assert_eq!(user.value(), "ada");
                assert_eq!(pass.value(), "hunter2");
            }
            other => panic!("expected AuthEdit::Basic, got {other:?}"),
        }
        assert_eq!(
            edit.auth_field_order(),
            &[AuthField::BasicUser, AuthField::BasicPass]
        );
    }

    #[test]
    fn auth_edit_new_seeds_api_key_name_value_and_location() {
        let request = request_with_auth(
            "  api_key:\n    in: header\n    name: X-Api-Key\n    value: abc123\n",
        );

        let edit = EditState::new(&request);

        match &edit.auth {
            AuthEdit::ApiKey {
                name,
                value,
                location,
            } => {
                assert_eq!(name.value(), "X-Api-Key");
                assert_eq!(value.value(), "abc123");
                assert_eq!(*location, ApiKeyLocation::Header);
            }
            other => panic!("expected AuthEdit::ApiKey, got {other:?}"),
        }
        assert_eq!(
            edit.auth_field_order(),
            &[
                AuthField::ApiKeyName,
                AuthField::ApiKeyValue,
                AuthField::ApiKeyLocation
            ]
        );
    }

    #[test]
    fn auth_edit_new_seeds_oauth_fields_including_optional_ones() {
        let request = request_with_auth(
            "  oauth:\n    grant_type: password\n    token_url: https://auth.example.com/token\n    \
             client_id: my-client\n    client_secret: my-secret\n    scope: read write\n    \
             username: ada\n    password: hunter2\n",
        );

        let edit = EditState::new(&request);

        match &edit.auth {
            AuthEdit::OAuth {
                grant_type,
                token_url,
                client_id,
                client_secret,
                scope,
                username,
                password,
            } => {
                assert_eq!(*grant_type, OAuthGrantType::Password);
                assert_eq!(token_url.value(), "https://auth.example.com/token");
                assert_eq!(client_id.value(), "my-client");
                assert_eq!(client_secret.value(), "my-secret");
                assert_eq!(scope.value(), "read write");
                assert_eq!(username.value(), "ada");
                assert_eq!(password.value(), "hunter2");
            }
            other => panic!("expected AuthEdit::OAuth, got {other:?}"),
        }
    }

    #[test]
    fn auth_edit_to_auth_round_trips_a_bearer_token() {
        let request = request_with_auth("  bearer: secret-token\n");
        let edit = EditState::new(&request);

        let auth = edit.auth.to_auth().expect("bearer auth must round-trip");

        assert_eq!(auth.bearer.as_deref(), Some("secret-token"));
        assert_eq!(auth.basic, None);
        assert_eq!(auth.api_key, None);
        assert_eq!(auth.oauth, None);
    }

    #[test]
    fn auth_edit_to_auth_round_trips_api_key_after_toggling_location() {
        let request = request_with_auth(
            "  api_key:\n    in: header\n    name: X-Api-Key\n    value: abc123\n",
        );
        let mut edit = EditState::new(&request);
        edit.focus = EditField::Auth(AuthField::ApiKeyLocation);

        assert!(edit.toggle_focused(true));

        let auth = edit.auth.to_auth().expect("api_key auth must round-trip");
        let api_key = auth.api_key.expect("api_key must still be set");
        assert_eq!(
            api_key.r#in,
            ApiKeyLocation::Query,
            "toggling must have flipped header -> query"
        );
        assert_eq!(api_key.name, "X-Api-Key");
        assert_eq!(api_key.value, "abc123");
    }

    #[test]
    fn auth_edit_to_auth_clears_optional_oauth_fields_left_blank() {
        let request = request_with_auth(
            "  oauth:\n    grant_type: client_credentials\n    \
             token_url: https://auth.example.com/token\n    client_id: my-client\n    \
             client_secret: my-secret\n",
        );
        let edit = EditState::new(&request);

        let auth = edit.auth.to_auth().expect("oauth auth must round-trip");
        let oauth = auth.oauth.expect("oauth must still be set");
        assert_eq!(oauth.scope, None);
        assert_eq!(oauth.username, None);
        assert_eq!(oauth.password, None);
    }

    #[test]
    fn toggling_oauth_grant_type_flips_between_the_two_grants() {
        let request = request_with_auth(
            "  oauth:\n    grant_type: client_credentials\n    \
             token_url: https://auth.example.com/token\n    client_id: my-client\n    \
             client_secret: my-secret\n",
        );
        let mut edit = EditState::new(&request);
        edit.focus = EditField::Auth(AuthField::OAuthGrantType);

        assert!(edit.toggle_focused(true));
        let auth = edit.auth.to_auth().unwrap();
        assert_eq!(
            auth.oauth.as_ref().unwrap().grant_type,
            OAuthGrantType::Password
        );

        assert!(edit.toggle_focused(true));
        let auth = edit.auth.to_auth().unwrap();
        assert_eq!(
            auth.oauth.as_ref().unwrap().grant_type,
            OAuthGrantType::ClientCredentials
        );
    }

    #[test]
    fn toggle_focused_is_a_no_op_off_a_toggle_field() {
        let request = request_with_auth("  bearer: secret-token\n");
        let mut edit = EditState::new(&request);
        edit.focus = EditField::Auth(AuthField::BearerToken);

        assert!(
            !edit.toggle_focused(true),
            "a plain text auth field has nothing to toggle"
        );
    }

    #[test]
    fn typing_into_the_focused_bearer_token_field_edits_the_working_copy() {
        let request = request_with_auth("  bearer: old-token\n");
        let mut edit = EditState::new(&request);
        edit.focus = EditField::Auth(AuthField::BearerToken);

        let field = edit
            .focused_field_mut()
            .expect("bearer token is a real text field");
        for _ in 0.."old-token".len() {
            field.backspace();
        }
        field.insert_char('x');

        match &edit.auth {
            AuthEdit::Bearer { token } => assert_eq!(token.value(), "x"),
            other => panic!("expected AuthEdit::Bearer, got {other:?}"),
        }
    }

    #[test]
    fn focused_field_mut_is_none_for_a_fixed_enum_auth_sub_field() {
        let request = request_with_auth(
            "  api_key:\n    in: header\n    name: X-Api-Key\n    value: abc123\n",
        );
        let mut edit = EditState::new(&request);
        edit.focus = EditField::Auth(AuthField::ApiKeyLocation);

        assert!(
            edit.focused_field_mut().is_none(),
            "the location field has no TextField behind it"
        );
    }

    #[test]
    fn to_request_carries_the_edited_auth_into_a_candidate_request() {
        let request = request_with_auth("  bearer: old-token\n");
        let mut edit = EditState::new(&request);
        edit.focus = EditField::Auth(AuthField::BearerToken);
        let field = edit.focused_field_mut().unwrap();
        for _ in 0.."old-token".len() {
            field.backspace();
        }
        field.insert_char('n');
        field.insert_char('e');
        field.insert_char('w');

        let candidate = edit.to_request(&request);

        assert_eq!(candidate.auth.unwrap().bearer.as_deref(), Some("new"));
    }

    #[test]
    fn to_request_reflects_no_auth_when_the_request_has_none() {
        let request = request_from("");
        let edit = EditState::new(&request);

        let candidate = edit.to_request(&request);

        assert_eq!(candidate.auth, None);
    }

    // --- Assertion editing --------------------------------------------------

    fn request_with_assertions(assertions_yaml: &str) -> Request {
        let yaml = format!("method: GET\nurl: https://example.com\nassertions:\n{assertions_yaml}");
        Request::from_yaml_str(&yaml).expect("valid test request")
    }

    // --- JsonOperator ---

    #[test]
    fn json_operator_next_cycles_through_every_variant_and_wraps() {
        let mut op = JsonOperator::Equals;
        let mut seen = vec![op];
        for _ in 0..7 {
            op = op.next();
            seen.push(op);
        }
        assert_eq!(op.next(), JsonOperator::Equals, "must wrap back to Equals");
        assert_eq!(
            seen,
            vec![
                JsonOperator::Equals,
                JsonOperator::GreaterThan,
                JsonOperator::GreaterThanOrEqual,
                JsonOperator::LessThan,
                JsonOperator::LessThanOrEqual,
                JsonOperator::Contains,
                JsonOperator::Length,
                JsonOperator::Matches,
            ]
        );
    }

    #[test]
    fn json_operator_prev_is_the_exact_inverse_of_next() {
        for op in JsonOperator::ALL {
            assert_eq!(op.next().prev(), op);
            assert_eq!(op.prev().next(), op);
        }
    }

    #[test]
    fn json_operator_wrap_and_detect_round_trip_every_operator() {
        let arg = serde_json::json!(5);
        for op in JsonOperator::ALL {
            let wrapped = op.wrap(arg.clone());
            let (detected, detected_arg) = JsonOperator::detect(&wrapped);
            assert_eq!(detected, op, "wrap/detect must round-trip for {op:?}");
            assert_eq!(detected_arg, arg);
        }
    }

    #[test]
    fn json_operator_detect_reads_a_multi_key_object_as_equals() {
        // The documented disambiguation rule: only a single-key mapping with
        // a recognised key is an operator; anything else, including a
        // two-key object that happens to use an operator name, is equality.
        let value = serde_json::json!({"greater_than": 5, "less_than": 1});
        let (operator, arg) = JsonOperator::detect(&value);
        assert_eq!(operator, JsonOperator::Equals);
        assert_eq!(arg, value);
    }

    // --- AssertionRow / EditState::new seeding ---

    #[test]
    fn edit_state_new_is_empty_for_a_request_with_no_assertions() {
        let edit = EditState::new(&request_from(""));
        assert!(edit.assertions.is_empty());
        assert_eq!(edit.assertion_row_count(), 0);
    }

    #[test]
    fn edit_state_new_seeds_a_bare_equality_json_assertion() {
        let request = request_with_assertions("  json:\n    $.user.id: 42\n");
        let edit = EditState::new(&request);

        assert_eq!(edit.assertions.len(), 1);
        let row = &edit.assertions[0];
        assert_eq!(row.path.value(), "$.user.id");
        assert_eq!(row.operator, JsonOperator::Equals);
        assert_eq!(row.value.value(), "42");
        assert!(!row.negate);
        assert_eq!(row.value_error, None);
    }

    #[test]
    fn edit_state_new_seeds_a_string_equality_value_without_quotes() {
        let request = request_with_assertions("  json:\n    $.user.name: ada\n");
        let edit = EditState::new(&request);
        assert_eq!(edit.assertions[0].value.value(), "ada");
    }

    #[test]
    fn edit_state_new_seeds_an_operator_assertion_with_its_argument() {
        let request = request_with_assertions("  json:\n    $.count: {greater_than: 5}\n");
        let edit = EditState::new(&request);

        let row = &edit.assertions[0];
        assert_eq!(row.path.value(), "$.count");
        assert_eq!(row.operator, JsonOperator::GreaterThan);
        assert_eq!(row.value.value(), "5");
    }

    #[test]
    fn edit_state_new_seeds_both_json_and_not_json_rows_json_first() {
        let request =
            request_with_assertions("  json:\n    $.a: 1\n  not:\n    json:\n      $.b: 2\n");
        let edit = EditState::new(&request);

        assert_eq!(edit.assertions.len(), 2);
        assert_eq!(edit.assertions[0].path.value(), "$.a");
        assert!(!edit.assertions[0].negate);
        assert_eq!(edit.assertions[1].path.value(), "$.b");
        assert!(
            edit.assertions[1].negate,
            "the not: json entry must be marked negated"
        );
    }

    // --- add / delete rows ---

    #[test]
    fn add_assertion_row_appends_an_empty_row_and_focuses_its_path() {
        let mut edit = EditState::new(&request_from(""));

        edit.add_assertion_row();

        assert_eq!(edit.assertions.len(), 1);
        assert_eq!(edit.assertions[0].path.value(), "");
        assert_eq!(edit.assertions[0].operator, JsonOperator::Equals);
        assert_eq!(edit.focus, EditField::AssertionPath(0));
    }

    #[test]
    fn delete_focused_assertion_row_removes_it_and_focuses_the_previous_rows_path() {
        let request = request_with_assertions("  json:\n    $.a: 1\n    $.b: 2\n    $.c: 3\n");
        let mut edit = EditState::new(&request);
        edit.focus = EditField::AssertionValue(1); // $.b

        edit.delete_focused_assertion_row();

        assert_eq!(edit.assertions.len(), 2);
        assert_eq!(edit.assertions[0].path.value(), "$.a");
        assert_eq!(edit.assertions[1].path.value(), "$.c");
        assert_eq!(edit.focus, EditField::AssertionPath(0));
    }

    #[test]
    fn delete_focused_assertion_row_with_only_one_row_left_falls_back_before_assertions() {
        let request = request_with_assertions("  json:\n    $.a: 1\n");
        let mut edit = EditState::new(&request);
        edit.focus = EditField::AssertionPath(0);

        edit.delete_focused_assertion_row();

        assert!(edit.assertions.is_empty());
        assert_eq!(
            edit.focus,
            EditField::Body,
            "a request with no body: at all still gets an empty editable Body field, so focus \
             lands there rather than on Url"
        );
    }

    #[test]
    fn delete_focused_assertion_row_is_a_no_op_when_focus_is_elsewhere() {
        let request = request_with_assertions("  json:\n    $.a: 1\n");
        let mut edit = EditState::new(&request);
        edit.focus = EditField::Method;

        edit.delete_focused_assertion_row();

        assert_eq!(edit.assertions.len(), 1, "nothing should be deleted");
        assert_eq!(edit.focus, EditField::Method);
    }

    // --- value validation ---

    #[test]
    fn validate_assertion_value_text_accepts_ordinary_yaml_scalars_and_collections() {
        assert!(validate_assertion_value_text("42").is_ok());
        assert!(validate_assertion_value_text("ada").is_ok());
        assert!(validate_assertion_value_text("[a, b]").is_ok());
        assert!(
            validate_assertion_value_text("").is_ok(),
            "blank parses as null"
        );
    }

    #[test]
    fn validate_assertion_value_text_rejects_malformed_yaml() {
        assert!(validate_assertion_value_text("[a, b").is_err());
    }

    #[test]
    fn toggling_the_operator_recomputes_nothing_but_the_row_and_cycles_both_directions() {
        let mut edit = EditState::new(&request_with_assertions("  json:\n    $.a: 1\n"));
        edit.focus = EditField::AssertionOperator(0);

        assert!(edit.toggle_focused(true));
        assert_eq!(edit.assertions[0].operator, JsonOperator::GreaterThan);

        assert!(edit.toggle_focused(false));
        assert_eq!(edit.assertions[0].operator, JsonOperator::Equals);
    }

    #[test]
    fn toggling_negate_flips_the_flag_regardless_of_direction() {
        let mut edit = EditState::new(&request_with_assertions("  json:\n    $.a: 1\n"));
        edit.focus = EditField::AssertionNegate(0);

        assert!(edit.toggle_focused(true));
        assert!(edit.assertions[0].negate);

        assert!(edit.toggle_focused(false));
        assert!(!edit.assertions[0].negate);
    }

    // --- to_assertions round-trip ---

    #[test]
    fn to_assertions_saves_an_added_row_into_json() {
        let request = request_from("");
        let mut edit = EditState::new(&request);
        edit.add_assertion_row();
        edit.assertions[0].path = TextField::new("$.status");
        edit.assertions[0].value = TextField::new("ok");

        let assertions = edit
            .to_assertions(request.assertions.as_ref())
            .expect("a real assertion was added");

        assert_eq!(
            assertions.json.get("$.status"),
            Some(&serde_json::json!("ok"))
        );
        assert!(assertions.not.is_none());
    }

    #[test]
    fn to_assertions_saves_a_negated_row_into_not_json() {
        let request = request_from("");
        let mut edit = EditState::new(&request);
        edit.add_assertion_row();
        edit.assertions[0].path = TextField::new("$.status");
        edit.assertions[0].value = TextField::new("error");
        edit.assertions[0].negate = true;

        let assertions = edit
            .to_assertions(request.assertions.as_ref())
            .expect("a real assertion was added");

        assert!(assertions.json.is_empty());
        assert_eq!(
            assertions.not.unwrap().json.get("$.status"),
            Some(&serde_json::json!("error"))
        );
    }

    #[test]
    fn to_assertions_saves_an_operator_row_in_the_real_one_key_object_shape() {
        let request = request_from("");
        let mut edit = EditState::new(&request);
        edit.add_assertion_row();
        edit.assertions[0].path = TextField::new("$.count");
        edit.assertions[0].operator = JsonOperator::GreaterThanOrEqual;
        edit.assertions[0].value = TextField::new("10");

        let assertions = edit.to_assertions(request.assertions.as_ref()).unwrap();

        assert_eq!(
            assertions.json.get("$.count"),
            Some(&serde_json::json!({"greater_than_or_equal": 10}))
        );
    }

    #[test]
    fn to_assertions_editing_an_existing_row_replaces_its_value() {
        let request = request_with_assertions("  json:\n    $.a: 1\n");
        let mut edit = EditState::new(&request);
        edit.assertions[0].value = TextField::new("2");

        let assertions = edit.to_assertions(request.assertions.as_ref()).unwrap();

        assert_eq!(assertions.json.get("$.a"), Some(&serde_json::json!(2)));
    }

    #[test]
    fn to_assertions_deleting_the_only_row_returns_none_when_nothing_else_is_set() {
        let request = request_with_assertions("  json:\n    $.a: 1\n");
        let mut edit = EditState::new(&request);
        edit.focus = EditField::AssertionPath(0);
        edit.delete_focused_assertion_row();

        let assertions = edit.to_assertions(request.assertions.as_ref());

        assert_eq!(
            assertions, None,
            "an assertions: {{}} block must not be invented"
        );
    }

    #[test]
    fn to_assertions_preserves_every_other_assertion_kind_untouched() {
        // The scoping contract: status/status_in/headers/body_contains/
        // body_matches/elapsed_ms_under, and everything under `not:` besides
        // `json`, are not offered by this editor and must round-trip
        // completely unchanged.
        let request = request_with_assertions(
            "  status: 200\n  status_in: [200, 201]\n  headers:\n    accept: text/plain\n  \
             body_contains: ok\n  body_matches: 'ok$'\n  elapsed_ms_under: 500\n  json:\n    \
             $.a: 1\n  not:\n    status: 404\n    body_contains: error\n    json:\n      $.b: 2\n",
        );
        let mut edit = EditState::new(&request);
        // Edit the one thing this editor does offer, to prove the rest
        // survives a real save, not just an untouched no-op.
        edit.assertions[0].value = TextField::new("2");

        let assertions = edit
            .to_assertions(request.assertions.as_ref())
            .expect("still has plenty set");

        let original = request.assertions.as_ref().unwrap();
        assert_eq!(assertions.status, original.status);
        assert_eq!(assertions.status_in, original.status_in);
        assert_eq!(assertions.headers, original.headers);
        assert_eq!(assertions.body_contains, original.body_contains);
        assert_eq!(assertions.body_matches, original.body_matches);
        assert_eq!(assertions.elapsed_ms_under, original.elapsed_ms_under);
        let not = assertions.not.as_ref().unwrap();
        let original_not = original.not.as_ref().unwrap();
        assert_eq!(not.status, original_not.status);
        assert_eq!(not.body_contains, original_not.body_contains);
        // Only this one changed.
        assert_eq!(assertions.json.get("$.a"), Some(&serde_json::json!(2)));
    }

    #[test]
    fn to_request_carries_edited_assertions_into_a_candidate_request() {
        let request = request_with_assertions("  json:\n    $.a: 1\n");
        let mut edit = EditState::new(&request);
        edit.assertions[0].value = TextField::new("2");

        let candidate = edit.to_request(&request);

        assert_eq!(
            candidate.assertions.unwrap().json.get("$.a"),
            Some(&serde_json::json!(2))
        );
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
