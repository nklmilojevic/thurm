//! What a pane's terminal sends: keys (legacy, CSI/SS3 and kitty CSI u), bracketed paste, SGR
//! mouse reports and focus reports, as Neovim input.

/// A mouse report, for `nvim_input_mouse(button, action, modifier, grid, row, col)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mouse {
    /// "left", "right", "middle", "wheel", "move" (no button held), or "x1"/"x2".
    pub button: &'static str,
    /// "press", "drag", "release"; for "wheel": "up", "down", "left", "right"; "" for "move".
    pub action: &'static str,
    /// Neovim modifier prefixes, e.g. "C-A-S-" (Ctrl, Alt, Shift), or "".
    pub modifier: String,
    /// 0-based cell of the pane.
    pub row: u16,
    pub col: u16,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InputEvent {
    /// Keys in Neovim notation (`<C-w>`, `<lt>`, `a`, ...), for `nvim_input`.
    Keys(String),
    /// Bracketed paste: `PasteStart`, any number of `Paste` chunks, `PasteEnd`
    /// (`nvim_paste` phases 1, 2 and 3).
    PasteStart,
    Paste(String),
    PasteEnd,
    Mouse(Mouse),
    FocusIn,
    FocusOut,
}

/// Turns the bytes a pane's terminal sends into [`InputEvent`]s. Keeps state across reads: a
/// sequence may be split between two.
#[derive(Default)]
pub struct InputParser {}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// The events in `bytes`, after what earlier reads left unfinished.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        let _ = bytes;
        Vec::new()
    }
}
