//! A pane's screen as the page draws it: [`Frame`]s from `thurm-term` applied to a full grid,
//! as the apps' `Grid` does, and handed out as the rows that changed since the last draw.

use std::collections::HashMap;

use serde::Serialize;
use thurm_proto::{Cell, CursorState, Frame, FrameColors};

/// `u32`s per cell in [`Draw::cells`]: char, fg, bg, underline color, flags | link << 16.
pub const CELL_WORDS: usize = 5;

#[derive(Default)]
pub struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    clusters: HashMap<(u16, u16), String>,
    links: Vec<String>,
    dirty: Vec<bool>,
    meta: Meta,
}

/// Everything but the cells, for the page (JSON).
#[derive(Serialize, Clone, Default, Debug)]
pub struct Meta {
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorState,
    pub display_offset: u32,
    pub history_size: u32,
    pub modes: u32,
    pub colors: FrameColors,
    pub title: String,
}

/// What changed since the last [`Grid::take_draw`].
#[derive(Serialize, Default, Debug)]
pub struct Draw {
    #[serde(flatten)]
    pub meta: Meta,
    /// Rows to repaint, in order; their cells are in `cells`, `cols` cells each.
    pub dirty: Vec<u16>,
    /// `(row, col, text)` for cells of dirty rows whose grapheme is more than one `char`.
    pub clusters: Vec<(u16, u16, String)>,
    pub links: Vec<String>,
    #[serde(skip)]
    pub cells: Vec<u32>,
}

impl Grid {
    pub fn apply(&mut self, f: Frame) {
        let (cols, rows) = (f.cols as usize, f.rows as usize);
        if f.full || cols != self.cols || rows != self.rows {
            self.cols = cols;
            self.rows = rows;
            self.cells = vec![Cell::default(); cols * rows];
            self.clusters.clear();
            self.dirty = vec![true; rows];
        }
        if f.shift != 0 && !f.full {
            // Row r now shows what row r - shift showed; exposed rows follow in `lines`.
            let n = (f.shift.unsigned_abs() as usize).min(rows);
            let keep = (rows - n) * cols;
            if f.shift > 0 {
                self.cells.copy_within(0..keep, n * cols);
                self.cells[..n * cols].fill(Cell::default());
            } else {
                self.cells.copy_within(n * cols.., 0);
                self.cells[keep..].fill(Cell::default());
            }
            let s = f.shift as i64;
            self.clusters = std::mem::take(&mut self.clusters)
                .into_iter()
                .filter_map(|((row, col), v)| {
                    let to = row as i64 + s;
                    (0..rows as i64)
                        .contains(&to)
                        .then_some(((to as u16, col), v))
                })
                .collect();
            // A canvas can't move rows as cheaply as a texture: repaint them all.
            self.dirty.fill(true);
        }
        for line in f.lines {
            let r = line.row as usize;
            if r >= rows {
                continue;
            }
            self.clusters.retain(|(row, _), _| *row != line.row);
            for (c, cell) in line.cells.iter().enumerate().take(cols) {
                self.cells[r * cols + c] = *cell;
            }
            for (col, text) in line.clusters {
                self.clusters.insert((line.row, col), text);
            }
            self.dirty[r] = true;
        }
        if !f.links.is_empty() {
            self.links = f.links;
        }
        // The cursor and selection colors live in rows the frame may not resend.
        let m = &self.meta;
        if m.cursor != f.cursor
            || m.colors.selection_bg != f.colors.selection_bg
            || m.colors.cursor != f.colors.cursor
            || m.colors.cursor_text != f.colors.cursor_text
        {
            for row in [m.cursor.row as usize, f.cursor.row as usize] {
                if let Some(d) = self.dirty.get_mut(row) {
                    *d = true;
                }
            }
        }
        if m.colors.background != f.colors.background {
            self.dirty.fill(true);
        }
        self.meta = Meta {
            cols: f.cols,
            rows: f.rows,
            cursor: f.cursor,
            display_offset: f.display_offset,
            history_size: f.history_size,
            modes: f.modes,
            colors: f.colors,
            title: f.title,
        };
    }

    /// Marks every row for repainting (the page lost its canvas, or changed its font).
    pub fn invalidate(&mut self) {
        self.dirty.fill(true);
    }

    pub fn take_draw(&mut self) -> Draw {
        let mut draw = Draw {
            meta: self.meta.clone(),
            links: self.links.clone(),
            ..Default::default()
        };
        for (r, dirty) in self.dirty.iter_mut().enumerate() {
            if !std::mem::take(dirty) {
                continue;
            }
            draw.dirty.push(r as u16);
            for cell in &self.cells[r * self.cols..(r + 1) * self.cols] {
                draw.cells.extend_from_slice(&[
                    cell.ch as u32,
                    cell.fg,
                    cell.bg,
                    cell.ul,
                    cell.flags as u32 | (cell.link as u32) << 16,
                ]);
            }
        }
        let mut clusters: Vec<_> = self
            .clusters
            .iter()
            .filter(|((row, _), _)| draw.dirty.binary_search(row).is_ok())
            .map(|(&(row, col), text)| (row, col, text.clone()))
            .collect();
        clusters.sort();
        draw.clusters = clusters;
        draw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_proto::FrameRow;

    fn cell(ch: char) -> Cell {
        Cell {
            ch,
            ..Default::default()
        }
    }

    fn row(r: u16, text: &str) -> FrameRow {
        FrameRow {
            row: r,
            cells: text.chars().map(cell).collect(),
            clusters: Vec::new(),
        }
    }

    fn frame(full: bool, shift: i32, lines: Vec<FrameRow>) -> Frame {
        Frame {
            cols: 3,
            rows: 3,
            full,
            shift,
            lines,
            ..Default::default()
        }
    }

    fn text(draw: &Draw, i: usize) -> String {
        draw.cells[i * 3 * CELL_WORDS..(i + 1) * 3 * CELL_WORDS]
            .chunks(CELL_WORDS)
            .map(|c| char::from_u32(c[0]).unwrap_or('?'))
            .collect()
    }

    #[test]
    fn full_then_partial_frames() {
        let mut g = Grid::default();
        g.apply(frame(
            true,
            0,
            vec![row(0, "abc"), row(1, "def"), row(2, "ghi")],
        ));
        let d = g.take_draw();
        assert_eq!(d.dirty, [0, 1, 2]);
        assert_eq!(text(&d, 1), "def");
        assert!(g.take_draw().dirty.is_empty());
        g.apply(frame(false, 0, vec![row(1, "xyz")]));
        let d = g.take_draw();
        assert_eq!(d.dirty, [1]);
        assert_eq!(text(&d, 0), "xyz");
    }

    #[test]
    fn shifts_move_rows_and_repaint_all() {
        let mut g = Grid::default();
        g.apply(frame(
            true,
            0,
            vec![row(0, "abc"), row(1, "def"), row(2, "ghi")],
        ));
        g.take_draw();
        // Scrolled back one line: row 0 is new, the others moved down.
        g.apply(frame(false, 1, vec![row(0, "new")]));
        let d = g.take_draw();
        assert_eq!(d.dirty, [0, 1, 2]);
        assert_eq!(
            (text(&d, 0), text(&d, 1), text(&d, 2)),
            ("new".into(), "abc".into(), "def".into())
        );
    }

    #[test]
    fn a_new_cursor_color_repaints_the_cursor_row() {
        let mut g = Grid::default();
        g.apply(frame(
            true,
            0,
            vec![row(0, "abc"), row(1, "def"), row(2, "ghi")],
        ));
        g.take_draw();
        let mut f = frame(false, 0, vec![]);
        f.colors.cursor = 0xff0000;
        g.apply(f);
        assert_eq!(g.take_draw().dirty, [0]);
    }

    #[test]
    fn clusters_follow_their_rows() {
        let mut g = Grid::default();
        let mut r = row(0, "e  ");
        r.clusters.push((0, "e\u{301}".into()));
        g.apply(frame(true, 0, vec![r, row(1, "   "), row(2, "   ")]));
        assert_eq!(g.take_draw().clusters, [(0, 0, "e\u{301}".to_owned())]);
        g.apply(frame(false, 1, vec![row(0, "   ")]));
        assert_eq!(g.take_draw().clusters, [(1, 0, "e\u{301}".to_owned())]);
        g.apply(frame(false, 0, vec![row(1, "x  ")]));
        assert!(g.take_draw().clusters.is_empty());
    }
}
