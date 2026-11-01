//! A pane's [`Frame`] as terminal output: only the rows that changed since the last frame.

use crate::cell::{Cursor, CursorShape, Frame, FrameCell, Style, Underline};

/// Switches a pane's terminal to the modes thurm-nvim draws in: the alternate screen, bracketed
/// paste, focus reports, button-event mouse tracking with SGR reports, and the kitty keyboard
/// protocol (disambiguate + alternate keys). Autowrap is off: writing the last column of the
/// bottom row must not scroll the screen.
pub fn enter_modes(out: &mut Vec<u8>) {
    out.extend_from_slice(
        b"\x1b[?1049h\x1b[?2004h\x1b[?1004h\x1b[?1002h\x1b[?1006h\x1b[?7l\x1b[>5u\x1b[2J",
    );
}

/// Undoes [`enter_modes`]: back to the main screen, default cursor, colors and autowrap.
pub fn leave_modes(out: &mut Vec<u8>) {
    out.extend_from_slice(
        b"\x1b[<u\x1b[?1006l\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[?7h\x1b[0m\x1b[0 q\x1b[?25h\x1b[?1049l",
    );
}

/// Draws one pane's frames, remembering the last one to send only what changed.
#[derive(Default)]
pub struct PaneRenderer {
    /// The last frame drawn, which is what the terminal shows. `None` means draw in full.
    shown: Option<Frame>,
}

impl PaneRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget what is on the screen: the next frame is drawn in full (after a resize, or when
    /// the terminal may have been cleared).
    pub fn invalidate(&mut self) {
        self.shown = None;
    }

    /// Appends the output that turns the last frame into `frame` to `out`.
    pub fn draw(&mut self, frame: &Frame, out: &mut Vec<u8>) {
        let full = self
            .shown
            .as_ref()
            .is_none_or(|prev| prev.cols != frame.cols || prev.rows != frame.rows);
        out.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        if full {
            out.extend_from_slice(b"\x1b[0m\x1b[2J");
        }
        let mut emitted = None;
        for (row, line) in frame.lines.iter().enumerate() {
            let same = self.shown.as_ref().and_then(|prev| prev.lines.get(row)) == Some(line);
            if full || !same {
                draw_row(row, line, &mut emitted, out);
            }
        }
        if let Some(cursor) = frame.cursor {
            draw_cursor(cursor, out);
        }
        out.extend_from_slice(b"\x1b[?2026l");
        self.shown = Some(frame.clone());
    }
}

/// Writes one row from its first column, using `emitted` (the style last set) to skip SGR
/// that would change nothing.
fn draw_row(row: usize, line: &[FrameCell], emitted: &mut Option<Style>, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
    let erase = erase_start(line);
    let mut i = 0;
    while i < erase {
        let cell = &line[i];
        if cell.width == 0 && i > 0 && line[i - 1].width == 2 {
            // The right half of the wide character just written.
            i += 1;
            continue;
        }
        set_sgr(&cell.style, emitted, out);
        let text = cell.text.as_str();
        let fits = if cell.width == 2 {
            i + 1 < line.len()
        } else {
            cell.width == 1
        };
        if fits && !text.is_empty() {
            out.extend_from_slice(text.as_bytes());
        } else {
            out.push(b' ');
        }
        i += 1;
    }
    if erase < line.len() {
        set_sgr(&line[line.len() - 1].style, emitted, out);
        out.extend_from_slice(b"\x1b[K");
    }
}

/// Where a row's trailing run of blank cells starts, if `EL` can erase it: the run shares the
/// last cell's style, and that style must not decorate blanks. Otherwise the row's length.
fn erase_start(line: &[FrameCell]) -> usize {
    let Some(last) = line.last() else {
        return line.len();
    };
    if !last.text.is_blank() || last.style.decorates_blanks() {
        return line.len();
    }
    let mut start = line.len();
    while start > 0 && line[start - 1].text.is_blank() && line[start - 1].style == last.style {
        start -= 1;
    }
    // A wide character's right half belongs to the wide character, which is printed.
    if start < line.len() && line[start].width == 0 && start > 0 && line[start - 1].width == 2 {
        start += 1;
    }
    start
}

fn set_sgr(style: &Style, emitted: &mut Option<Style>, out: &mut Vec<u8>) {
    if emitted.as_ref() != Some(style) {
        out.extend_from_slice(sgr(style).as_bytes());
        *emitted = Some(*style);
    }
}

/// A full SGR sequence for `style`: it resets first, so it sets every attribute on its own.
fn sgr(style: &Style) -> String {
    let mut seq = String::from("\x1b[0");
    for (on, code) in [
        (style.bold, ";1"),
        (style.dim, ";2"),
        (style.italic, ";3"),
        (style.blink, ";5"),
        (style.reverse, ";7"),
        (style.conceal, ";8"),
        (style.strikethrough, ";9"),
        (style.overline, ";53"),
    ] {
        if on {
            seq.push_str(code);
        }
    }
    for (color, code) in [(style.fg, 38), (style.bg, 48), (style.sp, 58)] {
        if let Some(c) = color {
            seq.push_str(&format!(
                ";{code};2;{};{};{}",
                (c >> 16) & 0xff,
                (c >> 8) & 0xff,
                c & 0xff
            ));
        }
    }
    seq.push_str(match style.underline {
        Underline::None => "",
        Underline::Single => ";4:1",
        Underline::Double => ";4:2",
        Underline::Curly => ";4:3",
        Underline::Dotted => ";4:4",
        Underline::Dashed => ";4:5",
    });
    seq.push('m');
    seq
}

fn draw_cursor(cursor: Cursor, out: &mut Vec<u8>) {
    let shape = match (cursor.shape, cursor.blink) {
        (CursorShape::Block, false) => 2,
        (CursorShape::Block, true) => 1,
        (CursorShape::Horizontal, false) => 4,
        (CursorShape::Horizontal, true) => 3,
        (CursorShape::Vertical, false) => 6,
        (CursorShape::Vertical, true) => 5,
    };
    out.extend_from_slice(
        format!(
            "\x1b[{shape} q\x1b[0m\x1b[{};{}H\x1b[?25h",
            cursor.row as u32 + 1,
            cursor.col as u32 + 1
        )
        .as_bytes(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Text;
    use thurm_config::Theme;
    use thurm_proto::{CursorShape as ProtoShape, PaneSize, cell_flags as cf};
    use thurm_term::{ClientView, EngineConfig, TermMode, Terminal};

    fn emulator(cols: u16, rows: u16) -> Terminal {
        Terminal::new(
            PaneSize {
                cols,
                rows,
                cell_width: 10,
                cell_height: 20,
            },
            EngineConfig::default(),
        )
    }

    fn theme() -> Theme {
        EngineConfig::default().theme
    }

    fn screen(t: &mut Terminal) -> thurm_proto::Frame {
        t.snapshot(1, &mut ClientView::new())
            .expect("a full frame")
            .frame
    }

    fn draw(r: &mut PaneRenderer, frame: &Frame) -> Vec<u8> {
        let mut out = Vec::new();
        r.draw(frame, &mut out);
        out
    }

    fn count(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        count(hay, needle) > 0
    }

    /// The row numbers (1-based) of the cursor positioning sequences `CSI row ; col H`.
    fn cup_rows(out: &[u8]) -> Vec<u32> {
        let s = String::from_utf8_lossy(out);
        let mut rows = Vec::new();
        for part in s.split("\x1b[").skip(1) {
            let end = part
                .find(|c: char| !(c.is_ascii_digit() || c == ';'))
                .unwrap_or(part.len());
            let (params, rest) = part.split_at(end);
            if let Some(Ok(row)) = rest
                .starts_with('H')
                .then(|| params.split_once(';').map(|(row, _)| row.parse::<u32>()))
                .flatten()
            {
                rows.push(row);
            }
        }
        rows
    }

    /// Checks the emulator's screen cell by cell against `want`: the text, the colors (defaults
    /// filled in from the theme, reverse swapped), and the attribute flags. A wide character
    /// that does not fit in the last column is expected as a space.
    #[track_caller]
    fn assert_screen(t: &mut Terminal, want: &Frame) {
        let emu = screen(t);
        let theme = theme();
        assert_eq!((emu.cols, emu.rows), (want.cols, want.rows), "size");
        for (r, line) in want.lines.iter().enumerate() {
            let row = emu
                .lines
                .iter()
                .find(|l| l.row as usize == r)
                .expect("row in the emulator's frame");
            assert_eq!(row.cells.len(), want.cols as usize, "row {r} length");
            for (c, w) in line.iter().enumerate() {
                let e = row.cells[c];
                let at = format!("cell ({r},{c}) {w:?}");
                if w.width == 0 && c > 0 && line[c - 1].width == 2 {
                    assert!(
                        e.flags & cf::WIDE_SPACER != 0,
                        "{at}: right half of a wide char"
                    );
                    continue;
                }
                let wide = w.width == 2 && c + 1 < line.len();
                let text = if wide || w.width == 1 {
                    w.text.as_str()
                } else {
                    ""
                };
                let text = if text.is_empty() { " " } else { text };
                assert_eq!(e.ch.to_string(), text, "{at}: char");

                let (mut fg, mut bg) = (
                    w.style.fg.unwrap_or(theme.foreground),
                    w.style.bg.unwrap_or(theme.background),
                );
                if w.style.reverse {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if !w.style.dim {
                    assert_eq!(e.fg, fg, "{at}: fg");
                }
                assert_eq!(e.bg, bg, "{at}: bg");
                if let Some(sp) = w.style.sp {
                    assert_eq!(e.ul, sp, "{at}: underline color");
                }

                let underline = match w.style.underline {
                    Underline::None => 0,
                    Underline::Single => cf::UNDERLINE,
                    Underline::Double => cf::DOUBLE_UNDERLINE,
                    Underline::Curly => cf::UNDERCURL,
                    Underline::Dotted => cf::DOTTED_UNDERLINE,
                    Underline::Dashed => cf::DASHED_UNDERLINE,
                };
                assert_eq!(e.flags & cf::ANY_UNDERLINE, underline, "{at}: underline");
                for (flag, on) in [
                    (cf::BOLD, w.style.bold),
                    (cf::ITALIC, w.style.italic),
                    (cf::DIM, w.style.dim),
                    (cf::STRIKEOUT, w.style.strikethrough),
                    (cf::HIDDEN, w.style.conceal),
                    (cf::WIDE, wide),
                ] {
                    assert_eq!(e.flags & flag != 0, on, "{at}: flag {flag:#06x}");
                }
                assert_eq!(
                    e.flags & cf::DEFAULT_BG != 0,
                    w.style.bg.is_none() && !w.style.reverse,
                    "{at}: default background"
                );
            }
        }
    }

    #[track_caller]
    fn assert_cursor(t: &mut Terminal, want: Option<Cursor>) {
        let shown = t.mode().contains(TermMode::SHOW_CURSOR);
        let Some(c) = want else {
            assert!(!shown, "cursor should be hidden");
            return;
        };
        assert!(shown, "cursor should be shown");
        let emu = screen(t).cursor;
        assert_eq!((emu.row, emu.col), (c.row, c.col), "cursor position");
        assert_eq!(emu.blinking, c.blink, "cursor blink");
        let shape = match c.shape {
            CursorShape::Block => ProtoShape::Block,
            CursorShape::Horizontal => ProtoShape::Underline,
            CursorShape::Vertical => ProtoShape::Beam,
        };
        assert_eq!(emu.shape, shape, "cursor shape");
    }

    fn narrow(s: &str, style: Style) -> FrameCell {
        FrameCell {
            text: Text::new(s),
            width: 1,
            style,
        }
    }

    #[test]
    fn styled_full_frame_matches_the_emulator() {
        let mut want = Frame::blank(12, 3);
        want.put_str(
            0,
            0,
            "Hello",
            Style {
                fg: Some(0xff0000),
                bold: true,
                ..Default::default()
            },
        );
        want.put_str(
            0,
            6,
            "ul",
            Style {
                underline: Underline::Double,
                ..Default::default()
            },
        );
        want.put_str(
            1,
            0,
            "italic",
            Style {
                italic: true,
                underline: Underline::Curly,
                sp: Some(0x00ff00),
                bg: Some(0x203040),
                ..Default::default()
            },
        );
        want.put_str(
            2,
            0,
            "dim",
            Style {
                dim: true,
                ..Default::default()
            },
        );
        want.put_str(
            2,
            4,
            "rev",
            Style {
                reverse: true,
                fg: Some(0x112233),
                strikethrough: true,
                ..Default::default()
            },
        );

        let out = draw(&mut PaneRenderer::new(), &want);
        assert!(contains(&out, b"\x1b[2J"));
        assert!(contains(&out, b"\x1b[0;1;38;2;255;0;0m"));
        let mut t = emulator(12, 3);
        t.advance(&out);
        assert_screen(&mut t, &want);
        assert_cursor(&mut t, None);
    }

    #[test]
    fn wide_characters_take_two_columns_and_a_clipped_one_is_a_space() {
        let wide = |s: &str| FrameCell {
            text: Text::new(s),
            width: 2,
            style: Style::default(),
        };
        let spacer = FrameCell {
            text: Text::EMPTY,
            width: 0,
            style: Style::default(),
        };
        let blank = || narrow(" ", Style::default());
        let mut want = Frame::blank(6, 2);
        want.lines[0] = vec![
            narrow("a", Style::default()),
            wide("中"),
            spacer,
            narrow("b", Style::default()),
            wide("字"),
            spacer,
        ];
        // An orphaned right half after a narrow cell is a space, and the wide character at the
        // last column does not fit, so it is a space too.
        want.lines[1] = vec![
            narrow("x", Style::default()),
            spacer,
            blank(),
            blank(),
            blank(),
            wide("界"),
        ];

        let mut t = emulator(6, 2);
        t.advance(&draw(&mut PaneRenderer::new(), &want));
        assert_screen(&mut t, &want);
    }

    #[test]
    fn trailing_blanks_are_erased_in_their_background() {
        let blue = Style {
            bg: Some(0x336699),
            ..Default::default()
        };
        let underlined = Style {
            underline: Underline::Single,
            ..Default::default()
        };
        let mut want = Frame::blank(10, 4);
        want.put_str(0, 0, "ab", Style::default());
        for c in 2..10 {
            want.lines[0][c] = narrow(" ", blue);
        }
        // Blanks that show an underline are printed, not erased.
        want.put_str(1, 0, "cd", Style::default());
        for c in 2..10 {
            want.lines[1][c] = narrow(" ", underlined);
        }
        // Only the default-styled run at the end is erased; the blue blanks before it are not.
        want.put_str(2, 0, "x", Style::default());
        for c in 1..4 {
            want.lines[2][c] = narrow(" ", blue);
        }

        // Rows 0, 2 and 3 end in erasable blanks; row 1 does not.
        let out = draw(&mut PaneRenderer::new(), &want);
        assert_eq!(count(&out, b"\x1b[K"), 3);
        let mut t = emulator(10, 4);
        t.advance(&out);
        assert_screen(&mut t, &want);
    }

    #[test]
    fn unchanged_rows_are_not_sent_again() {
        let mut a = Frame::blank(8, 4);
        a.put_str(0, 0, "one", Style::default());
        a.put_str(1, 0, "two", Style::default());
        a.put_str(2, 0, "tri", Style::default());
        a.put_str(3, 0, "four", Style::default());
        let mut r = PaneRenderer::new();
        let mut t = emulator(8, 4);
        t.advance(&draw(&mut r, &a));

        let mut b = a.clone();
        b.put_str(
            2,
            0,
            "changed",
            Style {
                bold: true,
                ..Default::default()
            },
        );
        let out = draw(&mut r, &b);
        assert_eq!(cup_rows(&out), vec![3]);
        assert!(!contains(&out, b"\x1b[2J"));
        t.advance(&out);
        assert_screen(&mut t, &b);

        let same = draw(&mut r, &b);
        assert!(cup_rows(&same).is_empty());
        t.advance(&same);
        assert_screen(&mut t, &b);
    }

    #[test]
    fn invalidate_and_resize_redraw_everything() {
        let mut a = Frame::blank(10, 3);
        a.put_str(0, 0, "top", Style::default());
        a.put_str(2, 0, "bottom", Style::default());
        let mut r = PaneRenderer::new();
        let mut t = emulator(10, 3);
        t.advance(&draw(&mut r, &a));

        r.invalidate();
        let again = draw(&mut r, &a);
        assert!(contains(&again, b"\x1b[2J"));
        assert_eq!(cup_rows(&again), vec![1, 2, 3]);
        t.advance(&again);
        assert_screen(&mut t, &a);

        let mut b = Frame::blank(12, 3);
        b.put_str(1, 0, "wider", Style::default());
        t.resize(PaneSize {
            cols: 12,
            rows: 3,
            cell_width: 10,
            cell_height: 20,
        });
        let out = draw(&mut r, &b);
        assert!(contains(&out, b"\x1b[2J"));
        assert_eq!(cup_rows(&out), vec![1, 2, 3]);
        t.advance(&out);
        assert_screen(&mut t, &b);
    }

    #[test]
    fn cursor_shape_position_and_visibility() {
        let mut want = Frame::blank(6, 2);
        let mut r = PaneRenderer::new();
        let mut t = emulator(6, 2);

        want.cursor = Some(Cursor {
            row: 1,
            col: 3,
            shape: CursorShape::Horizontal,
            blink: true,
        });
        let out = draw(&mut r, &want);
        assert!(out.ends_with(b"\x1b[3 q\x1b[0m\x1b[2;4H\x1b[?25h\x1b[?2026l"));
        t.advance(&out);
        assert_cursor(&mut t, want.cursor);

        for (shape, blink, code) in [
            (CursorShape::Block, false, 2),
            (CursorShape::Block, true, 1),
            (CursorShape::Horizontal, false, 4),
            (CursorShape::Horizontal, true, 3),
            (CursorShape::Vertical, false, 6),
            (CursorShape::Vertical, true, 5),
        ] {
            want.cursor = Some(Cursor {
                row: 0,
                col: 5,
                shape,
                blink,
            });
            let out = draw(&mut r, &want);
            assert!(contains(&out, format!("\x1b[{code} q").as_bytes()));
            t.advance(&out);
            assert_cursor(&mut t, want.cursor);
        }

        want.cursor = None;
        let out = draw(&mut r, &want);
        assert!(!contains(&out, b"\x1b[?25h"));
        t.advance(&out);
        assert_cursor(&mut t, None);
    }

    #[test]
    fn enter_and_leave_modes_set_and_restore_the_terminal() {
        let mut t = emulator(6, 2);
        let mut out = Vec::new();
        enter_modes(&mut out);
        t.advance(&out);
        assert!(!t.mode().contains(TermMode::LINE_WRAP));
        let entered = TermMode::ALT_SCREEN
            | TermMode::BRACKETED_PASTE
            | TermMode::FOCUS_IN_OUT
            | TermMode::MOUSE_DRAG
            | TermMode::SGR_MOUSE
            | TermMode::DISAMBIGUATE_ESC_CODES;
        assert!(t.mode().contains(entered));

        let mut out = Vec::new();
        leave_modes(&mut out);
        t.advance(&out);
        assert!(!t.mode().intersects(entered));
        assert!(t.mode().contains(TermMode::SHOW_CURSOR));
        assert!(t.mode().contains(TermMode::LINE_WRAP));
    }

    #[test]
    fn bottom_right_cell_does_not_scroll_the_screen() {
        let mut want = Frame::blank(4, 3);
        want.put_str(0, 0, "top", Style::default());
        want.put_str(1, 0, "mid", Style::default());
        want.put_str(2, 0, "bot", Style::default());
        want.put_str(2, 3, "z", Style::default());

        let mut enter = Vec::new();
        enter_modes(&mut enter);
        let mut t = emulator(4, 3);
        t.advance(&enter);
        t.advance(&draw(&mut PaneRenderer::new(), &want));
        assert_screen(&mut t, &want);
        assert_eq!(screen(&mut t).lines[0].cells[0].ch, 't');
    }

    #[test]
    fn sgr_sets_every_attribute_in_one_sequence() {
        let style = Style {
            fg: Some(0x010203),
            bg: Some(0xff0000),
            sp: Some(0x0000ff),
            bold: true,
            dim: true,
            italic: true,
            blink: true,
            reverse: true,
            conceal: true,
            strikethrough: true,
            overline: true,
            underline: Underline::Curly,
            ..Default::default()
        };
        assert_eq!(
            sgr(&style),
            "\x1b[0;1;2;3;5;7;8;9;53;38;2;1;2;3;48;2;255;0;0;58;2;0;0;255;4:3m"
        );
        assert_eq!(sgr(&Style::default()), "\x1b[0m");
    }
}
