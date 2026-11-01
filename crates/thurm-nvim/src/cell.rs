//! What a pane shows: cells of text with Neovim's highlights, and the frame a pane draws.

use std::collections::HashMap;

/// A grapheme cluster, stored inline. Longer ones than fit become U+FFFD.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Text {
    len: u8,
    bytes: [u8; Text::CAP],
}

impl Text {
    const CAP: usize = 23;

    pub const SPACE: Text = {
        let mut bytes = [0; Text::CAP];
        bytes[0] = b' ';
        Text { len: 1, bytes }
    };

    /// Nothing: the right half of a wide character.
    pub const EMPTY: Text = Text {
        len: 0,
        bytes: [0; Text::CAP],
    };

    pub fn new(s: &str) -> Text {
        let s = if s.len() > Self::CAP { "\u{fffd}" } else { s };
        let mut bytes = [0; Self::CAP];
        bytes[..s.len()].copy_from_slice(s.as_bytes());
        Text {
            len: s.len() as u8,
            bytes,
        }
    }

    pub fn as_str(&self) -> &str {
        // Only ever built from a `&str` that fit whole.
        std::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("\u{fffd}")
    }

    pub fn is_blank(&self) -> bool {
        self.len == 0 || self.as_str() == " "
    }
}

impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl Default for Text {
    fn default() -> Self {
        Text::SPACE
    }
}

/// One cell of a Neovim grid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub text: Text,
    /// Neovim's highlight id; 0 is the default colors.
    pub hl: u32,
    /// 1, 2 for a wide character, or 0 for the cell after one (its right half).
    pub width: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            text: Text::SPACE,
            hl: 0,
            width: 1,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// A highlight, as `hl_attr_define`'s rgb attributes. Colors are `0xRRGGBB`; `None` is the
/// default color (resolved by [`Styles::resolve`], or left to the terminal).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub struct Style {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub sp: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub reverse: bool,
    pub strikethrough: bool,
    pub dim: bool,
    pub blink: bool,
    pub conceal: bool,
    pub overline: bool,
    pub underline: Underline,
    /// 0-100: how much of what is under a floating window shows through.
    pub blend: u8,
}

impl Style {
    /// A blank cell in this style shows something besides its background, which `EL` would
    /// not draw.
    pub fn decorates_blanks(&self) -> bool {
        self.reverse || self.strikethrough || self.overline || self.underline != Underline::None
    }
}

/// Every highlight Neovim defined, and its default colors.
#[derive(Clone, Default, Debug)]
pub struct Styles {
    pub attrs: HashMap<u32, Style>,
    pub default_fg: Option<u32>,
    pub default_bg: Option<u32>,
    pub default_sp: Option<u32>,
}

impl Styles {
    /// Highlight `hl` with Neovim's default colors filled in where it has none. Colors still
    /// `None` are the terminal's own.
    pub fn resolve(&self, hl: u32) -> Style {
        self.resolve_style(self.attrs.get(&hl).copied().unwrap_or_default())
    }

    /// `s` with Neovim's default colors filled in where it has none.
    pub fn resolve_style(&self, mut s: Style) -> Style {
        s.fg = s.fg.or(self.default_fg);
        s.bg = s.bg.or(self.default_bg);
        s.sp = s.sp.or(self.default_sp);
        s
    }
}

/// A cell ready to draw: text with its style resolved.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct FrameCell {
    pub text: Text,
    /// 1, 2 for a wide character, or 0 for its right half (not drawn).
    pub width: u8,
    pub style: Style,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum CursorShape {
    #[default]
    Block,
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    pub blink: bool,
}

/// Everything a pane shows: `rows` lines of exactly `cols` cells, and the cursor if it is in
/// this pane.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Vec<FrameCell>>,
    pub cursor: Option<Cursor>,
}

impl Frame {
    /// A blank frame in the terminal's default colors.
    pub fn blank(cols: u16, rows: u16) -> Frame {
        Frame {
            cols,
            rows,
            lines: vec![
                vec![
                    FrameCell {
                        text: Text::SPACE,
                        width: 1,
                        style: Style::default()
                    };
                    cols as usize
                ];
                rows as usize
            ],
            cursor: None,
        }
    }

    /// Writes `s` at `row`, `col` in `style`, one cell per char (tests and simple text).
    #[cfg(test)]
    pub fn put_str(&mut self, row: u16, col: u16, s: &str, style: Style) {
        let Some(line) = self.lines.get_mut(row as usize) else {
            return;
        };
        for (i, ch) in s.chars().enumerate() {
            if let Some(c) = line.get_mut(col as usize + i) {
                let mut buf = [0; 4];
                *c = FrameCell {
                    text: Text::new(ch.encode_utf8(&mut buf)),
                    width: 1,
                    style,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_keeps_clusters_and_replaces_long_ones() {
        assert_eq!(Text::new("é").as_str(), "é");
        assert_eq!(Text::new("👩‍👩‍👧").as_str(), "👩‍👩‍👧");
        assert_eq!(Text::new(&"a\u{301}".repeat(12)).as_str(), "\u{fffd}");
        assert!(Text::SPACE.is_blank() && Text::EMPTY.is_blank() && !Text::new("x").is_blank());
    }

    #[test]
    fn styles_fill_in_default_colors() {
        let mut styles = Styles {
            default_fg: Some(0xcdd6f4),
            default_bg: Some(0x1e1e2e),
            ..Default::default()
        };
        styles.attrs.insert(
            3,
            Style {
                fg: Some(0xff0000),
                bold: true,
                ..Default::default()
            },
        );
        let s = styles.resolve(3);
        assert_eq!((s.fg, s.bg, s.bold), (Some(0xff0000), Some(0x1e1e2e), true));
        assert_eq!(styles.resolve(99).fg, Some(0xcdd6f4));
    }
}
