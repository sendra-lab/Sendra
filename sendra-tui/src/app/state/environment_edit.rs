//! The state of an in-progress edit of one environment's variables, opened
//! from the environment overlay — `EnvironmentEditState` and its own
//! `EnvVarField`/`PendingEnvVarDelete` companions. A sibling of
//! `super::edit::EditState`, not a variant of it — see `EnvironmentEditState`'s
//! own doc comment for why the two can never even coexist.

use sendra_core::Environment;

use super::edit::{HeaderRow, TextField};
use super::ConfirmPrompt;

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
    pub(crate) fn new(index: usize, environment: &Environment) -> Self {
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
    pub(crate) fn focused_field_mut(&mut self) -> Option<&mut TextField> {
        let row = match self.focus? {
            EnvVarField::Name(index) => return self.rows.get_mut(index).map(|row| &mut row.key),
            EnvVarField::Value(index) => index,
        };
        self.rows.get_mut(row).map(|row| &mut row.value)
    }

    /// `Tab`: name → value → next row's name, wrapping from the last row's
    /// value back to the first row's name. A no-op when `rows` is empty.
    pub(crate) fn focus_next(&mut self) {
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
    pub(crate) fn focus_prev(&mut self) {
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
    pub(crate) fn add_row(&mut self) {
        self.rows.push(HeaderRow::default());
        self.focus = Some(EnvVarField::Name(self.rows.len() - 1));
    }

    /// Marks whichever row `focus` currently points at pending deletion — a
    /// no-op when nothing is focused. The actual removal is
    /// [`Self::confirm_pending_delete`]; this only opens the confirmation.
    /// Visible to `super::update`'s `Message::RequestDeleteEnvVarRow` arm.
    pub(crate) fn request_delete_focused(&mut self) {
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
    pub(crate) fn cancel_pending_delete(&mut self) {
        self.pending_delete = None;
    }

    /// Actually removes the row named by `pending_delete`, if any — visible
    /// to `super::update`'s `Message::ConfirmDeleteEnvVarRow` arm. Focus
    /// afterward never dangles on a removed row: it moves to the row that
    /// slid into its place (or the new last row, if the deleted one was
    /// last), or to `None` if no rows remain — the zero-variables state this
    /// type's own `focus` doc comment describes.
    pub(crate) fn confirm_pending_delete(&mut self) {
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
