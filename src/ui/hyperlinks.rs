//! Drawing a pane's links as real hyperlinks in the outer terminal.
//!
//! Outside CST a link Copilot prints is an OSC 8 hyperlink: the terminal shows where it
//! goes on hover and opens it on Ctrl+click. Inside CST the pane is redrawn from a cell
//! grid, and neither `vt100` nor ratatui carries hyperlinks, so the link reached the
//! screen as underlined text with nothing behind it.
//!
//! This puts them back. After a frame is drawn, the cells of each link are printed again,
//! exactly as ratatui just drew them, between an OSC 8 open and close. The characters and
//! colours land on themselves, so nothing visibly changes — but the outer terminal now
//! knows those cells are a link. That is what makes a click open it on the machine the
//! user is sitting at, which is the only one that matters over SSH: CST running on a
//! remote host has no way to reach the local browser except through its own output.
//!
//! Only web links are drawn this way. A terminal opens a `file:` link with whatever the
//! file's type launches, so a link to an `.exe` would run it — CST keeps those to itself
//! and reveals them in their folder instead. See [`crate::links`].

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;

/// One link to draw: where it goes, and each cell it covers with what that cell should
/// read.
///
/// The expected text is checked against the finished frame before anything is printed.
/// Anything CST drew on top — a popup, the command palette — changes those cells, and a
/// link half-hidden under a dialog is left alone rather than reprinted over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlinkRun {
    pub target: String,
    /// `(column, row, symbol)` in outer-terminal coordinates.
    pub cells: Vec<(u16, u16, String)>,
}

const OPEN_PREFIX: &str = "\x1b]8;;";
const TERMINATOR: &str = "\x1b\\";

/// The bytes that turn the drawn cells of each run into a hyperlink.
///
/// Empty when there is nothing to draw, so a frame with no links writes nothing extra.
pub fn overlay(runs: &[HyperlinkRun], frame: &Buffer) -> Vec<u8> {
    let mut out = Vec::new();
    for run in runs {
        let cells: Option<Vec<_>> = run
            .cells
            .iter()
            .map(|(x, y, expected)| {
                frame
                    .cell((*x, *y))
                    .filter(|cell| same_symbol(cell.symbol(), expected))
                    .map(|cell| (*x, *y, cell))
            })
            .collect();
        let Some(cells) = cells.filter(|cells| !cells.is_empty()) else {
            continue;
        };
        // The target was classified as a web address, which rules out control characters,
        // so it cannot end the sequence early or smuggle another one in.
        out.extend_from_slice(OPEN_PREFIX.as_bytes());
        out.extend_from_slice(run.target.as_bytes());
        out.extend_from_slice(TERMINATOR.as_bytes());
        // Reprinted by ratatui's own backend, so the colours, underline and wide
        // characters come out exactly as the frame just drew them.
        let _ = CrosstermBackend::new(&mut out).draw(cells.into_iter());
        out.extend_from_slice(OPEN_PREFIX.as_bytes());
        out.extend_from_slice(TERMINATOR.as_bytes());
    }
    out
}

/// A cell `vt100` never wrote reads as nothing; the same cell drawn reads as a space.
fn same_symbol(drawn: &str, expected: &str) -> bool {
    let normalise = |symbol: &str| if symbol.is_empty() { " " } else { symbol }.to_string();
    normalise(drawn) == normalise(expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};

    fn frame_with(text: &str, x: u16, y: u16) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 5));
        buffer.set_string(x, y, text, Style::default().fg(Color::Cyan));
        buffer
    }

    fn run(target: &str, text: &str, x: u16, y: u16) -> HyperlinkRun {
        HyperlinkRun {
            target: target.to_string(),
            cells: text
                .chars()
                .enumerate()
                .map(|(i, c)| (x + i as u16, y, c.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_visible_link_is_reprinted_inside_a_hyperlink() {
        let frame = frame_with("see docs", 2, 1);
        let bytes = overlay(
            &[run("https://example.com/a?b=1&c=2", "docs", 6, 1)],
            &frame,
        );
        let text = String::from_utf8(bytes).unwrap();

        let open = text
            .find("\x1b]8;;https://example.com/a?b=1&c=2\x1b\\")
            .expect("opened");
        let word = text.find("docs").expect("the link text is reprinted");
        let close = text.rfind("\x1b]8;;\x1b\\").expect("closed");
        assert!(open < word && word < close, "got {text:?}");
    }

    #[test]
    fn a_link_hidden_under_something_else_is_left_alone() {
        // A popup has been drawn over the link's cells since the pane painted them.
        let frame = frame_with("POPUP!", 6, 1);
        let bytes = overlay(&[run("https://example.com", "docs", 6, 1)], &frame);
        assert!(bytes.is_empty(), "never reprint a link over a dialog");
    }

    #[test]
    fn a_frame_with_no_links_writes_nothing_extra() {
        assert!(overlay(&[], &frame_with("plain", 0, 0)).is_empty());
    }
}
