//! The Sendra mark, hand-built from plain terminal characters rather than
//! decoded from an image.
//!
//! ratatui has no portable way to show a *real* image: terminal graphics
//! protocols (Sixel, Kitty's, iTerm2's own inline-image scheme) are each
//! supported by only a handful of terminals with no common fallback, so this
//! crate never reaches for one. A PNG decoded and downsampled into colored
//! half-block characters was tried first and dropped: at the tiny size a
//! terminal pane can actually show, the real logo's antialiased curves and
//! background just read as color noise rather than a recognizable mark, no
//! matter how faithfully the pixels were sampled. This is a small, stylized
//! abstraction of the same composition instead — an arrow, two tick-mark
//! accents, and a rounded loop with a bar through it, echoing the real
//! mark's arrow-and-loop shape — built entirely from ordinary box-drawing
//! characters, so it renders identically, and legibly, in any terminal.

use ratatui::text::Line;

use super::theme;

/// Each row of the mark, rendered as one `Paragraph`
/// (`view::welcome::render_plain_welcome`) with `Alignment::Center` — which
/// centers every row independently by its own width, so rows of differing
/// length will not stay aligned with each other the way `MARK` intends the
/// shape to read; keep every row the same character width to avoid that.
const MARK: &[&str] = &[
    "                              ███▄",
    "        ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄█████▄",
    "       ██████████████████████████████",
    "       ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀████▀",
    "▄▄▄▄   ▄▄▄▄▄▄▄▄▄▄▄▄▄▄          ▀▀▀",
    "█████  ██████████████",
    " ▀▀▀   ▀▀▀▀▀▀▀▀▀▀▀▀▀",
    "",
    "       ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄",
    "       ▀██████████████████████▄",
    "         ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀██████",
    "               ▄▄▄▄▄▄▄▄▄▄  ▀█████",
    "              ████████████  █████",
    "              ▀▀▀▀▀▀▀▀▀▀▀▀  █████",
    " ▄▄▄   ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄█████",
    "█████  ████████████████████████▀",
    "▀▀▀▀   ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
];

/// The mark, built into `Line`s ready for a `Paragraph` — every line the
/// same [`theme::brand`] green, the app's own fixed identity color. Cheap
/// enough (six short, `const` strings) to build fresh on every call rather
/// than caching: there is no decode step left to amortize now that this is
/// no longer an image.
pub(crate) fn logo_lines() -> Vec<Line<'static>> {
    MARK.iter()
        .map(|row| Line::styled(*row, theme::brand()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_lines_returns_one_line_per_row_with_its_own_text_styled_brand_green() {
        let lines = logo_lines();
        assert_eq!(lines.len(), MARK.len());
        for (line, row) in lines.iter().zip(MARK.iter()) {
            // A blank separator row (`""` in `MARK`) has no text to hold a
            // span at all — `Line::styled("", ..)` produces zero spans, not
            // one empty one — so this only checks content on a non-empty
            // row.
            if !row.is_empty() {
                assert_eq!(line.spans.len(), 1);
                assert_eq!(line.spans[0].content, *row);
            }
            // `Line::styled` sets the *line's* own style, not each span's —
            // ratatui's own rendering (`Line::styled_graphemes`) patches
            // every span's style on top of this at render time, so this is
            // the field that actually determines what color reaches the
            // screen, not `line.spans[0].style` (which stays `Style::new()`,
            // the unstyled default `Span::raw` always starts with).
            assert_eq!(line.style, theme::brand());
        }
    }
}
