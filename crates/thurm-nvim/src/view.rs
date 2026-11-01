//! What one pane shows: its window's grid, its statusline, the floats over it, and in the
//! pane of Neovim's current window, the message grid (command line, messages) at the bottom.

use crate::cell::{Cursor, CursorShape, Frame, FrameCell, Style, Text};
use crate::grid::{EDITOR, Grid, Model};

/// One pane to compose.
pub struct PaneView<'a> {
    pub cols: u16,
    pub rows: u16,
    /// The grid of the window it shows (none yet: blank).
    pub grid: Option<u64>,
    /// It shows Neovim's current window: the message grid, editor floats and the command line
    /// cursor go here. The editor is sized to this pane, so editor coordinates are this
    /// pane's.
    pub current: bool,
    /// Its window's statusline cells, drawn on the row above the message line.
    pub status: Option<&'a [FrameCell]>,
}

impl PaneView<'_> {
    /// Rows left for the window's text: all but the message line and the statusline.
    pub fn text_rows(&self) -> u16 {
        self.rows
            .saturating_sub(1 + self.status.is_some() as u16)
            .max(1)
    }
}

pub fn compose(model: &Model, p: &PaneView) -> Frame {
    let mut frame = Frame::blank(p.cols, p.rows);
    let text_rows = p.text_rows();
    let resolve = |hl| model.styles.resolve(hl);
    let blank = FrameCell {
        text: Text::SPACE,
        width: 1,
        style: resolve(0),
    };
    for line in &mut frame.lines {
        line.fill(blank);
    }

    let window = p.grid.and_then(|g| model.grids.get(&g));
    if let Some(g) = window {
        blit(&mut frame, g, 0, 0, text_rows, &resolve, None);
    }
    if let Some(status) = p.status
        && text_rows < p.rows
    {
        let line = &mut frame.lines[text_rows as usize];
        let fill = status.last().map(|c| c.style).unwrap_or_default();
        for (i, cell) in line.iter_mut().enumerate() {
            *cell = status.get(i).copied().unwrap_or(FrameCell {
                text: Text::SPACE,
                width: 1,
                style: fill,
            });
        }
    }

    let mut cursor = None;
    let cursor_at = |row: i32, col: i32, limit: u16| {
        (row >= 0 && col >= 0 && (row as u16) < limit && (col as u16) < p.cols)
            .then_some((row as u16, col as u16))
    };
    if let Some(g) = p.grid
        && model.cursor_grid == g
    {
        cursor = cursor_at(model.cursor.0 as i32, model.cursor.1 as i32, text_rows);
    }

    // The message grid: rows from `msg_row` of the editor, which is this pane's size.
    if p.current
        && let Some(mg) = model.msg_grid
        && let Some(msg) = model.grids.get(&mg)
    {
        let top = model.msg_row.clamp(0, p.rows as i64) as i32;
        let visible = (p.rows as i32 - top).min(msg.rows as i32).max(0) as u16;
        blit(
            &mut frame,
            msg,
            top,
            0,
            top as u16 + visible,
            &resolve,
            None,
        );
        if model.cursor_grid == mg {
            cursor = cursor_at(top + model.cursor.0 as i32, model.cursor.1 as i32, p.rows);
        }
    }

    // Floats: those over this window, and over the editor in the current window's pane.
    let mut floats = p.grid.map(|g| model.floats_on(g)).unwrap_or_default();
    if p.current {
        floats.extend(model.floats_on(EDITOR));
        floats.sort_by_key(|(g, _)| match model.wins.get(g).map(|w| &w.kind) {
            Some(crate::grid::WinKind::Float(pos)) => (pos.zindex, pos.compindex),
            _ => (0, 0),
        });
    }
    for (fg, at) in floats {
        let Some(g) = model.grids.get(&fg) else {
            continue;
        };
        blit(&mut frame, g, at.row, at.col, p.rows, &resolve, Some(()));
        if model.cursor_grid == fg {
            cursor = cursor_at(
                at.row + model.cursor.0 as i32,
                at.col + model.cursor.1 as i32,
                p.rows,
            );
        }
    }

    mend_wide(&mut frame);
    if !model.busy {
        let mode = model.mode_info();
        frame.cursor = cursor.map(|(row, col)| Cursor {
            row,
            col,
            shape: if model.cursor_style_enabled {
                mode.shape
            } else {
                CursorShape::Block
            },
            blink: model.cursor_style_enabled && mode.blink,
        });
    }
    frame
}

/// Copies grid `g` onto `frame` at (`row`, `col`), above rows `limit`. A float (`float` set)
/// blends with what is under it where its highlight has `blend`.
fn blit(
    frame: &mut Frame,
    g: &Grid,
    row: i32,
    col: i32,
    limit: u16,
    resolve: &dyn Fn(u32) -> Style,
    float: Option<()>,
) {
    for r in 0..g.rows {
        let fr = row + r as i32;
        if fr < 0 || fr >= limit.min(frame.rows) as i32 {
            continue;
        }
        let line = &mut frame.lines[fr as usize];
        for (c, cell) in g.row(r).iter().enumerate() {
            let fc = col + c as i32;
            if fc < 0 || fc >= frame.cols as i32 {
                continue;
            }
            let under = line[fc as usize];
            let style = resolve(cell.hl);
            let mut out = FrameCell {
                text: cell.text,
                width: cell.width,
                style,
            };
            if float.is_some() && style.blend > 0 {
                out = blend(out, under, style.blend);
            }
            line[fc as usize] = out;
        }
    }
}

/// A float cell over `under`, `blend` percent see-through: the background mixes, and where the
/// float is blank, the text under it shows through in a mixed color.
fn blend(over: FrameCell, under: FrameCell, blend: u8) -> FrameCell {
    let mix = |a: Option<u32>, b: Option<u32>| match (a, b) {
        (Some(a), Some(b)) => Some(mix_rgb(a, b, blend)),
        (a, b) => a.or(b),
    };
    let mut style = over.style;
    style.bg = mix(over.style.bg, under.style.bg);
    if over.text.is_blank() && over.width == 1 && under.width == 1 {
        style.fg = mix(over.style.bg, under.style.fg);
        return FrameCell {
            text: under.text,
            width: 1,
            style,
        };
    }
    FrameCell { style, ..over }
}

/// `a` with `t` percent of `b`.
fn mix_rgb(a: u32, b: u32, t: u8) -> u32 {
    let t = t.min(100) as u32;
    let ch = |shift: u32| {
        let (x, y) = ((a >> shift) & 0xff, (b >> shift) & 0xff);
        ((x * (100 - t) + y * t) / 100) << shift
    };
    ch(16) | ch(8) | ch(0)
}

/// A wide character cut by a float's edge leaves a half: blank it.
fn mend_wide(frame: &mut Frame) {
    for line in &mut frame.lines {
        let n = line.len();
        for i in 0..n {
            let ok = match line[i].width {
                2 => i + 1 < n && line[i + 1].width == 0,
                0 => i > 0 && line[i - 1].width == 2,
                _ => true,
            };
            if !ok {
                line[i].text = Text::SPACE;
                line[i].width = 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{FloatPos, Win, WinKind};

    fn grid(lines: &[&str]) -> Grid {
        let rows = lines.len() as u16;
        let cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let mut g = Grid {
            cols,
            rows,
            cells: vec![crate::cell::Cell::default(); cols as usize * rows as usize],
        };
        for (r, l) in lines.iter().enumerate() {
            for (c, ch) in l.chars().enumerate() {
                g.cells[r * cols as usize + c].text = Text::new(&ch.to_string());
            }
        }
        g
    }

    fn rows(f: &Frame) -> Vec<String> {
        f.lines
            .iter()
            .map(|l| l.iter().map(|c| c.text.as_str()).collect::<String>())
            .collect()
    }

    fn model() -> Model {
        let mut m = Model::default();
        m.grids
            .insert(2, grid(&["hello world", "second line", "third", ""]));
        m.wins.insert(
            2,
            Win {
                win: 1000,
                kind: WinKind::External,
            },
        );
        m
    }

    #[test]
    fn window_status_and_message_line() {
        let mut m = model();
        m.grids.insert(3, grid(&[":w"]));
        m.msg_grid = Some(3);
        m.msg_row = 5;
        m.cursor_grid = 3;
        m.cursor = (0, 2);
        let status: Vec<FrameCell> = "main.rs"
            .chars()
            .map(|c| FrameCell {
                text: Text::new(&c.to_string()),
                width: 1,
                style: Style::default(),
            })
            .collect();
        let view = PaneView {
            cols: 11,
            rows: 6,
            grid: Some(2),
            current: true,
            status: Some(&status),
        };
        assert_eq!(view.text_rows(), 4);
        let f = compose(&m, &view);
        assert_eq!(
            rows(&f),
            [
                "hello world",
                "second line",
                "third      ",
                "           ",
                "main.rs    ",
                ":w         "
            ]
        );
        assert_eq!(f.cursor.map(|c| (c.row, c.col)), Some((5, 2)));

        // Not the current window's pane: no message line, no cursor there.
        let other = compose(
            &m,
            &PaneView {
                current: false,
                ..view
            },
        );
        assert_eq!(rows(&other)[5], "           ");
        assert_eq!(other.cursor, None);
    }

    #[test]
    fn floats_cover_and_blend() {
        let mut m = model();
        m.grids.insert(4, grid(&["[x]"]));
        m.wins.insert(
            4,
            Win {
                win: 1001,
                kind: WinKind::Float(FloatPos {
                    anchor: "NW".into(),
                    anchor_grid: 2,
                    row: 1.0,
                    col: 2.0,
                    focusable: true,
                    zindex: 50,
                    compindex: 0,
                    screen: None,
                }),
            },
        );
        m.styles.attrs.insert(
            9,
            Style {
                bg: Some(0x000000),
                blend: 50,
                ..Default::default()
            },
        );
        m.styles.default_bg = Some(0xffffff);
        let f = compose(
            &m,
            &PaneView {
                cols: 11,
                rows: 5,
                grid: Some(2),
                current: false,
                status: None,
            },
        );
        assert_eq!(rows(&f)[1], "se[x]d line");

        // A blank see-through float cell shows the text under it, mixed.
        m.grids.get_mut(&4).unwrap().cells[1] = crate::cell::Cell {
            text: Text::SPACE,
            hl: 9,
            width: 1,
        };
        let f = compose(
            &m,
            &PaneView {
                cols: 11,
                rows: 5,
                grid: Some(2),
                current: false,
                status: None,
            },
        );
        assert_eq!(rows(&f)[1], "se[o]d line");
        assert_eq!(f.lines[1][3].style.bg, Some(0x7f7f7f));
    }

    #[test]
    fn a_float_cutting_a_wide_char_blanks_its_half() {
        let mut m = Model::default();
        let mut g = grid(&["ab  "]);
        g.cells[2] = crate::cell::Cell {
            text: Text::new("界"),
            hl: 0,
            width: 2,
        };
        g.cells[3] = crate::cell::Cell {
            text: Text::EMPTY,
            hl: 0,
            width: 0,
        };
        m.grids.insert(2, g);
        m.grids.insert(4, grid(&["X"]));
        m.wins.insert(
            4,
            Win {
                win: 1001,
                kind: WinKind::Float(FloatPos {
                    anchor: "NW".into(),
                    anchor_grid: 2,
                    row: 0.0,
                    col: 3.0,
                    focusable: false,
                    zindex: 50,
                    compindex: 0,
                    screen: None,
                }),
            },
        );
        let f = compose(
            &m,
            &PaneView {
                cols: 4,
                rows: 2,
                grid: Some(2),
                current: false,
                status: None,
            },
        );
        assert_eq!(rows(&f)[0], "ab X");
    }
}
