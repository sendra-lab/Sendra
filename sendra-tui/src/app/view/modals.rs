//! The shared confirmation prompt (`render_confirm_prompt`) every
//! destructive action in this crate is rendered through, and the
//! "open another collection" path-input prompt (`render_open_collection_prompt`)
//! in the same visual family.

use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use super::super::state::{ConfirmPrompt, OpenCollectionPromptState};
use super::{format_error, modal_frame};

/// The one confirmation-prompt component every destructive action in this
/// crate is rendered through — deleting a request, deleting an environment
/// variable, closing a tab, and quitting with unsaved work — mirroring how
/// [`format_error`] already gives every *error* one shared rendering instead
/// of each feature formatting its own. `heading` is the only thing that
/// varies per call site (`"Delete request"`, `"Close collection"`, ...);
/// everything else — the modal chrome (`modal_frame`, itself already unified
/// — see this issue's own audit notes), the "y/Enter confirm, n/Esc cancel"
/// wording, `prompt.message`, and `prompt.error` shown inline through the
/// same [`format_error`] every other error in the crate goes through — comes
/// from [`ConfirmPrompt`] and is built exactly once, here.
pub(crate) fn render_confirm_prompt(frame: &mut Frame, heading: &str, prompt: &ConfirmPrompt) {
    let inner = modal_frame(
        frame,
        60,
        30,
        format!("{heading} — y/Enter confirm, n/Esc cancel"),
    );

    let mut text = prompt.message.clone();
    if let Some(error) = &prompt.error {
        text.push_str("\n\n");
        text.push_str(&format_error("Failed", error));
    }
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

/// The "open another collection" path-input prompt — a minimal, single-field
/// overlay in the same visual family as [`render_confirm_prompt`] (built on
/// the same `modal_frame` chrome and the same inline-error convention), but
/// not itself a yes/no confirmation — there is nothing destructive about
/// typing a path — so it stays its own function rather than being forced
/// into [`ConfirmPrompt`]'s shape.
pub(crate) fn render_open_collection_prompt(frame: &mut Frame, prompt: &OpenCollectionPromptState) {
    let inner = modal_frame(
        frame,
        70,
        30,
        "Open collection — Enter to confirm, Esc to cancel",
    );

    let mut text = format!("Path: {}", prompt.path.value());
    if let Some(error) = &prompt.error {
        text.push_str("\n\n");
        text.push_str(&format_error("Failed to open", error));
    }
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}
