//! A pane's [`Frame`] as terminal output: only the rows that changed since the last frame.

use crate::cell::Frame;

/// Switches a pane's terminal to the modes thurm-nvim draws in: the alternate screen, bracketed
/// paste, focus reports, button-event mouse tracking with SGR reports, and the kitty keyboard
/// protocol (disambiguate + alternate keys).
pub fn enter_modes(out: &mut Vec<u8>) {
    let _ = out;
}

/// Undoes [`enter_modes`]: back to the main screen, default cursor and colors.
pub fn leave_modes(out: &mut Vec<u8>) {
    let _ = out;
}

/// Draws one pane's frames, remembering the last one to send only what changed.
#[derive(Default)]
pub struct PaneRenderer {}

impl PaneRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget what is on the screen: the next frame is drawn in full (after a resize, or when
    /// the terminal may have been cleared).
    pub fn invalidate(&mut self) {}

    /// Appends the output that turns the last frame into `frame` to `out`.
    pub fn draw(&mut self, frame: &Frame, out: &mut Vec<u8>) {
        let _ = (frame, out);
    }
}
