//! Neovim's screen as its `redraw` events describe it (`ext_linegrid` + `ext_multigrid`): one
//! grid per window, the message grid, and where each window and float sits.

use std::collections::HashMap;

use rmpv::Value;

use crate::cell::{Cell, CursorShape, Style, Styles, Text, Underline};
use crate::nvim::handle;

/// The default grid: the editor itself (the anchor windows, which no pane shows).
pub const EDITOR: u64 = 1;

#[derive(Clone, Default, Debug)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
}

impl Grid {
    pub fn row(&self, r: u16) -> &[Cell] {
        let c = self.cols as usize;
        let start = r as usize * c;
        self.cells.get(start..start + c).unwrap_or(&[])
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let mut cells = vec![Cell::default(); cols as usize * rows as usize];
        for r in 0..rows.min(self.rows) {
            for c in 0..cols.min(self.cols) {
                cells[r as usize * cols as usize + c as usize] =
                    self.cells[r as usize * self.cols as usize + c as usize];
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.cells = cells;
    }

    fn clear(&mut self) {
        self.cells.fill(Cell::default());
    }

    fn at(&mut self, r: u16, c: u16) -> Option<&mut Cell> {
        if r >= self.rows || c >= self.cols {
            return None;
        }
        self.cells
            .get_mut(r as usize * self.cols as usize + c as usize)
    }

    /// `grid_scroll`: rows `top..bot`, columns `left..right` move up by `n` (down when
    /// negative); the rows scrolled in keep their old content, as Neovim redraws them.
    fn scroll(&mut self, top: u16, bot: u16, left: u16, right: u16, n: i64) {
        let (bot, right) = (bot.min(self.rows), right.min(self.cols));
        let width = right.saturating_sub(left) as usize;
        let cols = self.cols as usize;
        let copy = |cells: &mut Vec<Cell>, from: u16, to: u16| {
            let (f, t) = (
                from as usize * cols + left as usize,
                to as usize * cols + left as usize,
            );
            cells.copy_within(f..f + width, t);
        };
        if n > 0 {
            for r in top..bot.saturating_sub(n as u16) {
                copy(&mut self.cells, r + n as u16, r);
            }
        } else if n < 0 {
            let n = (-n) as u16;
            for r in (top + n..bot).rev() {
                copy(&mut self.cells, r - n, r);
            }
        }
    }
}

/// Where a float hangs: `anchor` corner of the float at (`row`, `col`) of `anchor_grid`.
#[derive(Clone, Debug, PartialEq)]
pub struct FloatPos {
    pub anchor: String,
    pub anchor_grid: u64,
    pub row: f64,
    pub col: f64,
    pub focusable: bool,
    pub zindex: i64,
    pub compindex: i64,
    /// Where Neovim draws it on the editor, kept inside it (`screen_row`, `screen_col`; Neovim
    /// 0.10 and later). Only meaningful for a float on the editor.
    pub screen: Option<(i64, i64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WinKind {
    /// In the editor's own layout (an anchor, or a window not yet made external).
    Ordinary,
    /// External: shown in a pane of its own.
    External,
    Float(FloatPos),
    Hidden,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Win {
    /// Neovim's window handle.
    pub win: i64,
    pub kind: WinKind,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct ModeInfo {
    pub shape: CursorShape,
    pub blink: bool,
}

/// Where a float lands: its top left cell on its root grid (a window's grid, or the editor),
/// and that root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub root: u64,
    pub row: i32,
    pub col: i32,
}

#[derive(Default, Debug)]
pub struct Model {
    pub grids: HashMap<u64, Grid>,
    /// grid -> its window.
    pub wins: HashMap<u64, Win>,
    pub styles: Styles,
    pub modes: Vec<ModeInfo>,
    pub mode: usize,
    pub cursor_style_enabled: bool,
    pub busy: bool,
    pub cursor_grid: u64,
    pub cursor: (u16, u16),
    /// The message grid, and its row on the editor grid.
    pub msg_grid: Option<u64>,
    pub msg_row: i64,
}

impl Model {
    /// Applies a `redraw` notification's batches; true when it ended with `flush`.
    pub fn apply_redraw(&mut self, params: &[Value]) -> bool {
        let mut flushed = false;
        for batch in params {
            let Value::Array(batch) = batch else { continue };
            let Some(name) = batch.first().and_then(Value::as_str) else {
                continue;
            };
            for args in &batch[1..] {
                let Value::Array(args) = args else { continue };
                if self.event(name, args) {
                    flushed = true;
                }
            }
        }
        flushed
    }

    fn event(&mut self, name: &str, a: &[Value]) -> bool {
        let u = |i: usize| a.get(i).and_then(Value::as_u64).unwrap_or(0);
        let i = |i: usize| a.get(i).and_then(Value::as_i64).unwrap_or(0);
        match name {
            "grid_resize" => self
                .grids
                .entry(u(0))
                .or_default()
                .resize(u(1) as u16, u(2) as u16),
            "grid_clear" => {
                if let Some(g) = self.grids.get_mut(&u(0)) {
                    g.clear();
                }
            }
            "grid_line" => self.grid_line(u(0), u(1) as u16, u(2) as u16, a.get(3)),
            "grid_scroll" => {
                if let Some(g) = self.grids.get_mut(&u(0)) {
                    g.scroll(u(1) as u16, u(2) as u16, u(3) as u16, u(4) as u16, i(5));
                }
            }
            "grid_cursor_goto" => {
                self.cursor_grid = u(0);
                self.cursor = (u(1) as u16, u(2) as u16);
            }
            "grid_destroy" => {
                self.grids.remove(&u(0));
                self.wins.remove(&u(0));
            }
            "win_pos" => self.set_win(u(0), a.get(1), WinKind::Ordinary),
            "win_external_pos" => self.set_win(u(0), a.get(1), WinKind::External),
            "win_float_pos" => {
                let pos = FloatPos {
                    anchor: a.get(2).and_then(Value::as_str).unwrap_or("NW").to_owned(),
                    anchor_grid: u(3),
                    row: a.get(4).and_then(Value::as_f64).unwrap_or(0.0),
                    col: a.get(5).and_then(Value::as_f64).unwrap_or(0.0),
                    focusable: a.get(6).and_then(Value::as_bool).unwrap_or(true),
                    zindex: i(7),
                    compindex: i(8),
                    screen: a
                        .get(9)
                        .and_then(Value::as_i64)
                        .zip(a.get(10).and_then(Value::as_i64)),
                };
                self.set_win(u(0), a.get(1), WinKind::Float(pos));
            }
            "win_hide" => {
                if let Some(w) = self.wins.get_mut(&u(0)) {
                    w.kind = WinKind::Hidden;
                }
            }
            "win_close" => {
                self.wins.remove(&u(0));
            }
            "msg_set_pos" => {
                self.msg_grid = Some(u(0));
                self.msg_row = i(1);
            }
            "hl_attr_define" => {
                if let Some(Value::Map(attrs)) = a.get(1) {
                    self.styles.attrs.insert(u(0) as u32, style(attrs));
                }
            }
            "default_colors_set" => {
                let color = |i: usize| {
                    a.get(i)
                        .and_then(Value::as_i64)
                        .filter(|c| *c >= 0)
                        .map(|c| c as u32)
                };
                self.styles.default_fg = color(0);
                self.styles.default_bg = color(1);
                self.styles.default_sp = color(2);
            }
            "mode_info_set" => {
                self.cursor_style_enabled = a.first().and_then(Value::as_bool).unwrap_or(false);
                if let Some(Value::Array(modes)) = a.get(1) {
                    self.modes = modes.iter().map(mode_info).collect();
                }
            }
            "mode_change" => self.mode = u(1) as usize,
            "busy_start" => self.busy = true,
            "busy_stop" => self.busy = false,
            "flush" => return true,
            _ => {}
        }
        false
    }

    fn set_win(&mut self, grid: u64, win: Option<&Value>, kind: WinKind) {
        let win = win.and_then(handle).unwrap_or(0);
        self.wins.insert(grid, Win { win, kind });
    }

    fn grid_line(&mut self, grid: u64, row: u16, col: u16, cells: Option<&Value>) {
        let (Some(g), Some(Value::Array(cells))) = (self.grids.get_mut(&grid), cells) else {
            return;
        };
        let mut c = col;
        let mut hl = 0;
        for cell in cells {
            let Value::Array(parts) = cell else { continue };
            let text = parts.first().and_then(Value::as_str).unwrap_or(" ");
            if let Some(h) = parts.get(1).and_then(Value::as_u64) {
                hl = h as u32;
            }
            let repeat = parts.get(2).and_then(Value::as_u64).unwrap_or(1);
            for _ in 0..repeat {
                // An empty cell is the right half of the wide character before it.
                if text.is_empty() {
                    if c > 0
                        && let Some(prev) = g.at(row, c - 1)
                    {
                        prev.width = 2;
                    }
                    if let Some(cell) = g.at(row, c) {
                        *cell = Cell {
                            text: Text::EMPTY,
                            hl,
                            width: 0,
                        };
                    }
                } else if let Some(cell) = g.at(row, c) {
                    *cell = Cell {
                        text: Text::new(text),
                        hl,
                        width: 1,
                    };
                }
                c += 1;
            }
        }
    }

    pub fn mode_info(&self) -> ModeInfo {
        self.modes.get(self.mode).copied().unwrap_or_default()
    }

    /// The grid showing window `win`.
    pub fn grid_of_win(&self, win: i64) -> Option<u64> {
        self.wins
            .iter()
            .find(|(_, w)| w.win == win)
            .map(|(g, _)| *g)
    }

    /// Where float grid `grid` lands: on the grid of the window it hangs from, or on the
    /// editor (floats on the message grid land on the editor at its row). A float on the
    /// editor is where Neovim draws it, kept inside (its `screen` position); one hanging from
    /// it builds on that, not on its parent's raw anchor.
    pub fn place(&self, grid: u64) -> Option<Placed> {
        self.place_within(grid, 8)
    }

    fn place_within(&self, g: u64, depth: u8) -> Option<Placed> {
        let Some(Win {
            kind: WinKind::Float(pos),
            ..
        }) = self.wins.get(&g)
        else {
            if Some(g) == self.msg_grid {
                return Some(Placed {
                    root: EDITOR,
                    row: self.msg_row as i32,
                    col: 0,
                });
            }
            return Some(Placed {
                root: g,
                row: 0,
                col: 0,
            });
        };
        let parent = self.place_within(pos.anchor_grid, depth.checked_sub(1)?)?;
        if parent.root == EDITOR
            && let Some((row, col)) = pos.screen
        {
            return Some(Placed {
                root: EDITOR,
                row: row as i32,
                col: col as i32,
            });
        }
        let size = self
            .grids
            .get(&g)
            .map(|x| (x.rows as f64, x.cols as f64))
            .unwrap_or((0.0, 0.0));
        let (dr, dc) = match pos.anchor.as_str() {
            "NE" => (0.0, size.1),
            "SW" => (size.0, 0.0),
            "SE" => (size.0, size.1),
            _ => (0.0, 0.0),
        };
        Some(Placed {
            root: parent.root,
            row: parent.row + (pos.row - dr).round() as i32,
            col: parent.col + (pos.col - dc).round() as i32,
        })
    }

    /// The floats (grid ids) that land on root grid `root`, bottom to top.
    pub fn floats_on(&self, root: u64) -> Vec<(u64, Placed)> {
        let mut out: Vec<(u64, Placed, i64, i64)> = self
            .wins
            .iter()
            .filter_map(|(g, w)| match &w.kind {
                WinKind::Float(pos) => {
                    let p = self.place(*g)?;
                    (p.root == root).then_some((*g, p, pos.zindex, pos.compindex))
                }
                _ => None,
            })
            .collect();
        out.sort_by_key(|(g, _, z, c)| (*z, *c, *g));
        out.into_iter().map(|(g, p, _, _)| (g, p)).collect()
    }
}

fn style(attrs: &[(Value, Value)]) -> Style {
    let mut s = Style::default();
    for (k, v) in attrs {
        let on = v.as_bool().unwrap_or(false);
        match k.as_str().unwrap_or("") {
            "foreground" => s.fg = v.as_u64().map(|c| c as u32),
            "background" => s.bg = v.as_u64().map(|c| c as u32),
            "special" => s.sp = v.as_u64().map(|c| c as u32),
            "bold" => s.bold = on,
            "italic" => s.italic = on,
            "reverse" => s.reverse = on,
            "strikethrough" => s.strikethrough = on,
            "dim" => s.dim = on,
            "blink" => s.blink = on,
            "conceal" => s.conceal = on,
            "overline" => s.overline = on,
            "underline" if on => s.underline = Underline::Single,
            "undercurl" if on => s.underline = Underline::Curly,
            "underdouble" if on => s.underline = Underline::Double,
            "underdotted" if on => s.underline = Underline::Dotted,
            "underdashed" if on => s.underline = Underline::Dashed,
            "blend" => s.blend = v.as_u64().unwrap_or(0).min(100) as u8,
            _ => {}
        }
    }
    s
}

/// A highlight as `nvim_get_hl` returns it (`fg`, `bg`, `sp`, `bold`, ...), for statuslines.
pub fn style_from_get_hl(attrs: &[(Value, Value)]) -> Style {
    let renamed: Vec<(Value, Value)> = attrs
        .iter()
        .map(|(k, v)| {
            let k = match k.as_str() {
                Some("fg") => "foreground",
                Some("bg") => "background",
                Some("sp") => "special",
                Some(other) => other,
                None => "",
            };
            (Value::from(k), v.clone())
        })
        .collect();
    style(&renamed)
}

fn mode_info(v: &Value) -> ModeInfo {
    let Value::Map(m) = v else {
        return ModeInfo::default();
    };
    let get = |key: &str| {
        m.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    };
    let shape = match get("cursor_shape").and_then(Value::as_str) {
        Some("horizontal") => CursorShape::Horizontal,
        Some("vertical") => CursorShape::Vertical,
        _ => CursorShape::Block,
    };
    let nonzero = |key: &str| get(key).and_then(Value::as_u64).is_some_and(|n| n != 0);
    ModeInfo {
        shape,
        blink: nonzero("blinkon") && nonzero("blinkoff"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str, args: Vec<Value>) -> Value {
        Value::Array(vec![name.into(), Value::Array(args)])
    }

    fn line(cells: &[(&str, Option<u64>, Option<u64>)]) -> Value {
        Value::Array(
            cells
                .iter()
                .map(|(t, hl, rep)| {
                    let mut v = vec![Value::from(*t)];
                    if let Some(h) = hl {
                        v.push((*h).into());
                        if let Some(r) = rep {
                            v.push((*r).into());
                        }
                    }
                    Value::Array(v)
                })
                .collect(),
        )
    }

    fn text(g: &Grid, r: u16) -> String {
        g.row(r).iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn lines_scrolls_and_wide_chars() {
        let mut m = Model::default();
        let flushed = m.apply_redraw(&[
            ev("grid_resize", vec![2.into(), 6.into(), 3.into()]),
            ev(
                "grid_line",
                vec![
                    2.into(),
                    0.into(),
                    0.into(),
                    line(&[
                        ("a", Some(5), None),
                        ("b", None, None),
                        (" ", Some(0), Some(4)),
                    ]),
                    false.into(),
                ],
            ),
            ev(
                "grid_line",
                vec![
                    2.into(),
                    1.into(),
                    0.into(),
                    line(&[("界", Some(0), None), ("", None, None), ("x", None, None)]),
                    false.into(),
                ],
            ),
            Value::Array(vec!["flush".into(), Value::Array(vec![])]),
        ]);
        assert!(flushed);
        let g = &m.grids[&2];
        assert_eq!(text(g, 0), "ab    ");
        assert_eq!((g.row(0)[1].hl, g.row(0)[2].hl), (5, 0));
        assert_eq!((g.row(1)[0].width, g.row(1)[1].width), (2, 0));
        assert_eq!(text(g, 1), "界x   ");

        m.apply_redraw(&[ev(
            "grid_scroll",
            vec![
                2.into(),
                0.into(),
                3.into(),
                0.into(),
                6.into(),
                1.into(),
                0.into(),
            ],
        )]);
        assert_eq!(text(&m.grids[&2], 0), "界x   ");
        m.apply_redraw(&[ev("grid_resize", vec![2.into(), 4.into(), 2.into()])]);
        assert_eq!(text(&m.grids[&2], 0), "界x ");
    }

    #[test]
    fn floats_land_on_their_root() {
        let mut m = Model::default();
        m.apply_redraw(&[
            ev("grid_resize", vec![2.into(), 40.into(), 10.into()]),
            ev("grid_resize", vec![4.into(), 10.into(), 3.into()]),
            ev("grid_resize", vec![5.into(), 4.into(), 2.into()]),
            ev("win_external_pos", vec![2.into(), 1000.into()]),
            ev(
                "win_float_pos",
                vec![
                    4.into(),
                    1001.into(),
                    "NW".into(),
                    2.into(),
                    2.0.into(),
                    5.0.into(),
                    true.into(),
                    50.into(),
                ],
            ),
            ev(
                "win_float_pos",
                vec![
                    5.into(),
                    1002.into(),
                    "SE".into(),
                    4.into(),
                    3.0.into(),
                    10.0.into(),
                    false.into(),
                    60.into(),
                ],
            ),
            ev(
                "msg_set_pos",
                vec![6.into(), 23.into(), false.into(), "".into()],
            ),
            ev(
                "win_float_pos",
                vec![
                    7.into(),
                    1003.into(),
                    "NW".into(),
                    6.into(),
                    0.0.into(),
                    0.0.into(),
                    false.into(),
                    70.into(),
                ],
            ),
        ]);
        assert_eq!(
            m.place(4),
            Some(Placed {
                root: 2,
                row: 2,
                col: 5
            })
        );
        // SE corner at (3, 10) of grid 4: its top left is 2 rows up, 4 columns left.
        assert_eq!(
            m.place(5),
            Some(Placed {
                root: 2,
                row: 3,
                col: 11
            })
        );
        assert_eq!(
            m.place(7),
            Some(Placed {
                root: EDITOR,
                row: 23,
                col: 0
            })
        );
        assert_eq!(
            m.floats_on(2).iter().map(|(g, _)| *g).collect::<Vec<_>>(),
            vec![4, 5]
        );
        assert_eq!(m.grid_of_win(1000), Some(2));

        // On the editor, Neovim's screen position wins over the anchor's (kept inside).
        m.apply_redraw(&[ev(
            "win_float_pos",
            vec![
                8.into(),
                1004.into(),
                "NW".into(),
                1.into(),
                5.0.into(),
                (-9.0).into(),
                true.into(),
                40.into(),
                0.into(),
                5.into(),
                0.into(),
            ],
        )]);
        assert_eq!(
            m.place(8),
            Some(Placed {
                root: EDITOR,
                row: 5,
                col: 0
            })
        );
        // A float hanging from it (no screen position of its own) builds on where it is drawn.
        m.apply_redraw(&[ev(
            "win_float_pos",
            vec![
                9.into(),
                1005.into(),
                "NW".into(),
                8.into(),
                1.0.into(),
                2.0.into(),
                true.into(),
                41.into(),
            ],
        )]);
        assert_eq!(
            m.place(9),
            Some(Placed {
                root: EDITOR,
                row: 6,
                col: 2
            })
        );
    }

    #[test]
    fn highlights_and_modes() {
        let mut m = Model::default();
        m.apply_redraw(&[
            ev(
                "default_colors_set",
                vec![
                    0xcdd6f4.into(),
                    0x1e1e2e.into(),
                    (-1).into(),
                    7.into(),
                    0.into(),
                ],
            ),
            ev(
                "hl_attr_define",
                vec![
                    3.into(),
                    Value::Map(vec![
                        ("foreground".into(), 0xff0000.into()),
                        ("bold".into(), true.into()),
                        ("undercurl".into(), true.into()),
                    ]),
                    Value::Map(vec![]),
                    Value::Array(vec![]),
                ],
            ),
            ev(
                "mode_info_set",
                vec![
                    true.into(),
                    Value::Array(vec![
                        Value::Map(vec![("cursor_shape".into(), "block".into())]),
                        Value::Map(vec![
                            ("cursor_shape".into(), "vertical".into()),
                            ("blinkon".into(), 250.into()),
                            ("blinkoff".into(), 400.into()),
                        ]),
                    ]),
                ],
            ),
            ev("mode_change", vec!["insert".into(), 1.into()]),
        ]);
        let s = m.styles.resolve(3);
        assert_eq!(
            (s.fg, s.bg, s.bold, s.underline),
            (Some(0xff0000), Some(0x1e1e2e), true, Underline::Curly)
        );
        assert_eq!(m.styles.default_sp, None);
        let mode = m.mode_info();
        assert_eq!((mode.shape, mode.blink), (CursorShape::Vertical, true));
    }
}
