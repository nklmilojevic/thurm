//! Draws a pane's grid with cairo, shaping text with pango. A port of Renderer.swift and
//! FontShaper.swift: each run of cells with one style is shaped in one call (ligatures, font
//! fallback), and every glyph is placed relative to its own cell, so fallback fonts and
//! ligatures never make the grid drift.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use gtk::cairo;
use gtk::pango;
use gtk::pango::prelude::*;
use pangocairo::prelude::PangoCairoFontExt as _;

use crate::boxdraw;
use crate::core::{Cell, GridInfo, ImagePlacement};

pub const FLAG_UNDERLINE: u16 = 1 << 2;
pub const FLAG_DOUBLE_UNDERLINE: u16 = 1 << 3;
pub const FLAG_UNDERCURL: u16 = 1 << 4;
pub const FLAG_DOTTED_UNDERLINE: u16 = 1 << 5;
pub const FLAG_DASHED_UNDERLINE: u16 = 1 << 6;
pub const FLAG_STRIKEOUT: u16 = 1 << 7;
pub const FLAG_WIDE: u16 = 1 << 8;
pub const FLAG_WIDE_SPACER: u16 = 1 << 9;
pub const FLAG_HIDDEN: u16 = 1 << 10;
pub const FLAG_SELECTED: u16 = 1 << 11;
pub const FLAG_SEARCH_MATCH: u16 = 1 << 12;
pub const FLAG_DEFAULT_BG: u16 = 1 << 13;
pub const FLAG_SEARCH_FOCUS: u16 = 1 << 15;

pub const NO_COLOR: u32 = 0xFF00_0000;

pub const MODE_SHOW_CURSOR: u32 = 1 << 0;
pub const MODE_BRACKETED_PASTE: u32 = 1 << 4;
pub const MODE_MOUSE_MOTION: u32 = 1 << 6;
pub const MODE_ALT_SCREEN: u32 = 1 << 12;
pub const MODE_MOUSE_ANY: u32 = (1 << 3) | (1 << 6) | (1 << 13);

pub const CURSOR_BLOCK: u8 = 0;
pub const CURSOR_UNDERLINE: u8 = 1;
pub const CURSOR_BEAM: u8 = 2;
pub const CURSOR_HOLLOW: u8 = 3;
pub const CURSOR_HIDDEN: u8 = 4;

const SEARCH_MATCH_BG: u32 = 0xD7BA5A;
const SEARCH_FOCUS_BG: u32 = 0xF0883E;
const SEARCH_FG: u32 = 0x1B1B1B;

pub const NERD_FONT: &str = "Symbols Nerd Font Mono";

// MARK: - Snapshot

/// A copy of a pane's screen, taken under the library's lock.
#[derive(Default, Clone)]
pub struct Snapshot {
    pub info: GridInfo,
    pub cells: Vec<Cell>,
    pub clusters: HashMap<(u16, u16), String>,
    pub links: HashMap<u16, String>,
    pub images: Vec<ImagePlacement>,
    pub peek: Option<(Vec<Cell>, HashMap<u16, String>)>,
    pub valid: bool,
}

impl Snapshot {
    pub fn cols(&self) -> usize {
        self.info.cols as usize
    }

    pub fn rows(&self) -> usize {
        self.info.rows as usize
    }

    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        let cols = self.cols();
        if col >= cols {
            return None;
        }
        self.cells.get(row * cols + col)
    }

    /// The row as text (wide spacers skipped, blanks as spaces) and each char's column.
    pub fn row_text(&self, row: usize) -> (String, Vec<u16>) {
        let mut text = String::new();
        let mut map = Vec::new();
        for c in 0..self.cols() {
            let Some(cell) = self.cell(row, c) else { break };
            if cell.flags & FLAG_WIDE_SPACER != 0 {
                continue;
            }
            match self.clusters.get(&(row as u16, c as u16)) {
                Some(s) => {
                    for ch in s.chars() {
                        text.push(ch);
                        map.push(c as u16);
                    }
                }
                None => {
                    let ch = char::from_u32(cell.ch).filter(|ch| *ch > ' ').unwrap_or(' ');
                    text.push(ch);
                    map.push(c as u16);
                }
            }
        }
        (text, map)
    }

    /// The URL at (row, col): an OSC 8 link, else a plain URL in the row text, with its span.
    pub fn link_at(&self, row: usize, col: usize) -> Option<(String, Hover)> {
        let cell = self.cell(row, col)?;
        if cell.link != 0
            && let Some(uri) = self.links.get(&cell.link)
        {
            return Some((uri.clone(), Hover::Link(cell.link)));
        }
        let (text, map) = self.row_text(row);
        let chars: Vec<char> = text.chars().collect();
        for (s, e) in crate::model::find_urls(&text) {
            let (c0, c1) = (map[s] as usize, map[e - 1] as usize);
            if (c0..=c1).contains(&col) {
                let url: String = chars[s..e].iter().collect();
                return Some((url, Hover::Span(row as u16, c0 as u16, c1 as u16)));
            }
        }
        None
    }
}

/// What the pointer hovers with the link modifier held: every cell of an OSC 8 link id, or a
/// detected URL's columns on one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Hover {
    #[default]
    None,
    Link(u16),
    Span(u16, u16, u16),
}

impl Hover {
    fn covers(&self, row: usize, col: usize, cell: &Cell) -> bool {
        match *self {
            Hover::None => false,
            Hover::Link(id) => cell.link == id,
            Hover::Span(r, a, b) => r as usize == row && (a as usize..=b as usize).contains(&col),
        }
    }
}

// MARK: - Fonts

struct FontEntry {
    scaled: cairo::ScaledFont,
    font: pango::Font,
    symbol: bool,
}

#[derive(Clone, Copy)]
struct ShapedGlyph {
    col: u16,
    x: f64,
    y: f64,
    font: usize,
    glyph: u32,
    /// Nerd Font icons fill 1 or 2 cells; 0 for ordinary glyphs.
    span: u8,
}

/// The four faces, the cell metrics, and the shaping caches.
pub struct Fonts {
    pub cell_w: f64,
    pub cell_h: f64,
    /// Baseline from the cell top.
    pub baseline: f64,
    pub line: f64,
    pub underline_offset: f64,
    pub strike_offset: f64,
    descs: [pango::FontDescription; 4],
    attrs: pango::AttrList,
    ctx: pango::Context,
    fonts: RefCell<Vec<FontEntry>>,
    /// By the pango font object (pango caches them): two fonts can share a description.
    font_ids: RefCell<HashMap<usize, usize>>,
    rows: RefCell<HashMap<Vec<u32>, Rc<Vec<ShapedGlyph>>>>,
    old_rows: RefCell<HashMap<Vec<u32>, Rc<Vec<ShapedGlyph>>>>,
}

impl Fonts {
    pub fn new(ctx: &pango::Context, cfg: &thurm_config::Config, features: &[String], size: f64) -> Fonts {
        let font = &cfg.font;
        let mut cascade: Vec<String> = vec![font.family.clone()];
        cascade.extend(font.fallback.iter().cloned());
        if font.nerd_font_symbols {
            cascade.push(NERD_FONT.into());
        }
        cascade.push("monospace".into());
        let face = |name: Option<&String>, weight: pango::Weight, style: pango::Style| {
            let mut d = pango::FontDescription::new();
            let mut families = cascade.clone();
            if let Some(n) = name {
                families.insert(0, n.clone());
            }
            d.set_family(&families.join(","));
            // Points are logical pixels, as on macOS (16 pt = 16 px at scale 1).
            d.set_absolute_size(size * pango::SCALE as f64);
            if name.is_none() {
                d.set_weight(weight);
                d.set_style(style);
            }
            d
        };
        let descs = [
            face(None, pango::Weight::Normal, pango::Style::Normal),
            face(font.family_bold.as_ref(), pango::Weight::Bold, pango::Style::Normal),
            face(font.family_italic.as_ref(), pango::Weight::Normal, pango::Style::Italic),
            face(
                font.family_bold_italic.as_ref().or(font.family_bold.as_ref()),
                pango::Weight::Bold,
                pango::Style::Italic,
            ),
        ];
        let attrs = pango::AttrList::new();
        let settings = feature_settings(features);
        if !settings.is_empty() {
            attrs.insert(pango::AttrFontFeatures::new(&settings));
        }

        let metrics = ctx.metrics(Some(&descs[0]), None);
        let s = pango::SCALE as f64;
        let ascent = metrics.ascent() as f64 / s;
        let descent = metrics.descent() as f64 / s;
        let layout = pango::Layout::new(ctx);
        layout.set_font_description(Some(&descs[0]));
        layout.set_text(&"M".repeat(64));
        let (_, logical) = layout.extents();
        let advance = logical.width() as f64 / s / 64.0;
        let letter_spacing = font.letter_spacing.clamp(-20.0, 100.0);
        let cell_w = (advance + letter_spacing).round().max(1.0);
        let natural = ascent + descent;
        let cell_h = (natural.ceil() * font.line_height.clamp(0.5, 4.0)).ceil().max(1.0);
        let top_pad = (cell_h - natural) / 2.0;
        let baseline = (top_pad + ascent).round().clamp(1.0, cell_h - 1.0);
        let line = (metrics.underline_thickness() as f64 / s).round().max(1.0);
        let mut underline_offset = (-(metrics.underline_position() as f64) / s).round().max(1.0);
        if baseline + underline_offset + line > cell_h {
            underline_offset = (cell_h - baseline - line).max(0.0);
        }
        let strike_offset = (metrics.strikethrough_position() as f64 / s).round().max(1.0);

        Fonts {
            cell_w,
            cell_h,
            baseline,
            line,
            underline_offset,
            strike_offset,
            descs,
            attrs,
            ctx: ctx.clone(),
            fonts: RefCell::new(Vec::new()),
            font_ids: RefCell::new(HashMap::new()),
            rows: RefCell::new(HashMap::new()),
            old_rows: RefCell::new(HashMap::new()),
        }
    }

    fn font_id(&self, font: &pango::Font) -> Option<usize> {
        let key = font.as_ptr() as usize;
        if let Some(&id) = self.font_ids.borrow().get(&key) {
            return Some(id);
        }
        let scaled = font.downcast_ref::<pangocairo::Font>()?.scaled_font()?;
        let family = font.describe().family().map(|f| f.to_string()).unwrap_or_default();
        let mut fonts = self.fonts.borrow_mut();
        fonts.push(FontEntry {
            scaled,
            font: font.clone(),
            symbol: family == NERD_FONT,
        });
        let id = fonts.len() - 1;
        self.font_ids.borrow_mut().insert(key, id);
        Some(id)
    }

    /// The glyphs of one row; cached by the row's content.
    fn shape_row(
        &self,
        cells: &[Cell],
        clusters: &dyn Fn(u16) -> Option<String>,
    ) -> Rc<Vec<ShapedGlyph>> {
        let mut key: Vec<u32> = Vec::with_capacity(cells.len() + 1);
        let mut any = false;
        let mut cluster_text = Vec::new();
        for (c, cell) in cells.iter().enumerate() {
            if cell.flags & FLAG_WIDE_SPACER != 0 {
                key.push(u32::MAX);
                continue;
            }
            let cl = clusters(c as u16);
            let scalar = shaped_scalar(cell);
            if scalar != 32 || cl.is_some() {
                any = true;
            }
            key.push(
                scalar
                    | (((cell.flags & 3) as u32) << 21)
                    | if cell.flags & FLAG_WIDE != 0 { 1 << 23 } else { 0 },
            );
            if let Some(s) = cl {
                cluster_text.push((c as u16, s));
            }
        }
        if !any {
            return Rc::new(Vec::new());
        }
        for (c, s) in &cluster_text {
            key.push(0xFFFF_0000 | *c as u32);
            key.extend(s.chars().map(|ch| ch as u32));
        }
        if let Some(hit) = self.rows.borrow().get(&key) {
            return hit.clone();
        }
        if let Some(hit) = self.old_rows.borrow_mut().remove(&key) {
            self.rows.borrow_mut().insert(key, hit.clone());
            return hit;
        }

        let cluster_at = |c: u16| cluster_text.iter().find(|(cc, _)| *cc == c).map(|(_, s)| s.as_str());
        let mut out = Vec::new();
        // Style runs: spaces never break a run (ligatures may span them).
        let mut text = String::new();
        let mut map: Vec<u16> = Vec::new(); // byte → column
        let mut space: Vec<bool> = Vec::new(); // byte → from a blank cell
        let mut style = 0u16;
        let blank_after = |c: usize| {
            cells.get(c + 1).is_some_and(|n| {
                n.flags & FLAG_WIDE_SPACER == 0
                    && shaped_scalar(n) == 32
                    && cluster_at((c + 1) as u16).is_none()
            })
        };
        let mut flush = |text: &mut String, map: &mut Vec<u16>, space: &mut Vec<bool>, style: u16| {
            if !text.is_empty() && space.iter().any(|s| !s) {
                self.shape_run(text, map, space, style, &blank_after, &mut out);
            }
            text.clear();
            map.clear();
            space.clear();
        };
        for (c, cell) in cells.iter().enumerate() {
            if cell.flags & FLAG_WIDE_SPACER != 0 {
                continue;
            }
            let cl = cluster_at(c as u16);
            let scalar = shaped_scalar(cell);
            if scalar == 32 && cl.is_none() {
                text.push(' ');
                map.push(c as u16);
                space.push(true);
                continue;
            }
            let st = cell.flags & 3;
            if st != style && space.iter().any(|s| !s) {
                flush(&mut text, &mut map, &mut space, style);
            }
            style = st;
            let piece: String = match cl {
                Some(s) => s.to_string(),
                None => char::from_u32(scalar).map(String::from).unwrap_or_default(),
            };
            for _ in 0..piece.len() {
                map.push(c as u16);
                space.push(false);
            }
            text.push_str(&piece);
        }
        flush(&mut text, &mut map, &mut space, style);

        let shaped = Rc::new(out);
        let mut rows = self.rows.borrow_mut();
        if rows.len() >= 4096 {
            *self.old_rows.borrow_mut() = std::mem::take(&mut *rows);
        }
        rows.insert(key, shaped.clone());
        shaped
    }

    fn shape_run(
        &self,
        text: &str,
        map: &[u16],
        space: &[bool],
        style: u16,
        blank_after: &dyn Fn(usize) -> bool,
        out: &mut Vec<ShapedGlyph>,
    ) {
        let layout = pango::Layout::new(&self.ctx);
        layout.set_font_description(Some(&self.descs[(style & 3) as usize]));
        layout.set_attributes(Some(&self.attrs));
        layout.set_single_paragraph_mode(true);
        layout.set_text(text);
        let Some(line) = layout.line_readonly(0) else { return };
        let s = pango::SCALE as f64;
        let mut pen = 0.0;
        let mut cluster_start: HashMap<u16, f64> = HashMap::new();
        for run in line.runs() {
            let font = run.item().analysis().font();
            let gs = run.glyph_string();
            let infos = gs.glyph_info();
            // Pen position of every glyph (in glyph order, left to right).
            let mut xs = Vec::with_capacity(infos.len());
            let mut x = pen;
            for gi in infos {
                xs.push(x);
                x += gi.geometry().width() as f64 / s;
            }
            let Some(fid) = self.font_id(&font) else {
                pen = x;
                continue;
            };
            let symbol = self.fonts.borrow()[fid].symbol;
            // Clusters carry absolute byte ranges of the text: each one's column.
            let Ok(iter) = pango::GlyphItemIter::new_start(&run, text) else {
                pen = x;
                continue;
            };
            for (start_glyph, start_index, _, end_glyph, _, _) in iter {
                let si = start_index.max(0) as usize;
                if si >= map.len() || space[si] {
                    continue;
                }
                let col = map[si];
                let (a, b) = if start_glyph <= end_glyph {
                    (start_glyph, end_glyph)
                } else {
                    (end_glyph + 1, start_glyph + 1)
                };
                for gi in a.max(0) as usize..(b.max(0) as usize).min(infos.len()) {
                    let info = &infos[gi];
                    let glyph = info.glyph();
                    if glyph == pango::GLYPH_EMPTY || glyph & pango::GLYPH_UNKNOWN_FLAG != 0 {
                        continue;
                    }
                    let start = *cluster_start.entry(col).or_insert(xs[gi]);
                    let geo = info.geometry();
                    out.push(if symbol {
                        ShapedGlyph {
                            col,
                            x: 0.0,
                            y: 0.0,
                            font: fid,
                            glyph,
                            span: if blank_after(col as usize) { 2 } else { 1 },
                        }
                    } else {
                        ShapedGlyph {
                            col,
                            x: xs[gi] - start + geo.x_offset() as f64 / s,
                            y: geo.y_offset() as f64 / s,
                            font: fid,
                            glyph,
                            span: 0,
                        }
                    });
                }
            }
            pen = x;
        }
    }
}

/// `["-calt","ss01","cv01=2"]` → "calt=0,ss01=1,cv01=2"; anything not a 4-letter tag is
/// dropped.
pub fn feature_settings(features: &[String]) -> String {
    features
        .iter()
        .filter_map(|f| {
            let f = f.trim();
            let (tag, value) = if let Some(t) = f.strip_prefix('-') {
                (t, 0)
            } else if let Some((t, v)) = f.split_once('=') {
                (t, v.trim().parse().ok()?)
            } else {
                (f.strip_prefix('+').unwrap_or(f), 1)
            };
            (tag.len() == 4 && tag.chars().all(|c| c.is_ascii_alphanumeric()))
                .then(|| format!("{tag}={value}"))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The scalar to shape: blanks, controls, hidden cells and procedurally drawn code points
/// shape as a space.
fn shaped_scalar(cell: &Cell) -> u32 {
    let ch = cell.ch;
    if ch <= 32
        || ch == 0x7F
        || cell.flags & FLAG_HIDDEN != 0
        || ch > 0x10FFFF
        || (0xD800..=0xDFFF).contains(&ch)
        || boxdraw::is_procedural(ch)
    {
        32
    } else {
        ch
    }
}

// MARK: - Images

pub struct ImageEntry {
    pub serial: u64,
    pub surface: cairo::ImageSurface,
    pub last_used: Instant,
}

pub type ImageCache = HashMap<u32, ImageEntry>;

pub fn image_surface(w: i32, h: i32, argb: Vec<u8>) -> Option<cairo::ImageSurface> {
    let stride = cairo::Format::ARgb32.stride_for_width(w as u32).ok()?;
    let data = if stride == w * 4 {
        argb
    } else {
        let mut d = vec![0u8; (stride * h) as usize];
        for y in 0..h as usize {
            let src = &argb[y * w as usize * 4..(y + 1) * w as usize * 4];
            d[y * stride as usize..y * stride as usize + w as usize * 4].copy_from_slice(src);
        }
        d
    };
    cairo::ImageSurface::create_for_data(data, cairo::Format::ARgb32, w, h, stride).ok()
}

// MARK: - Painting

pub struct PaintParams<'a> {
    pub width: f64,
    pub height: f64,
    pub pad_x: f64,
    pub pad_y: f64,
    pub opacity: f64,
    pub focused: bool,
    pub cursor_on: bool,
    pub cursor_thickness: f64,
    /// Unfocused-split dim (0: none).
    pub dim: f64,
    /// Bell flash strength 0..1.
    pub flash: f64,
    /// Smooth scrolling: pixels the grid is drawn lower, with the peek line above.
    pub scroll_y: f64,
    pub hover: Hover,
    pub preedit: Option<&'a str>,
    pub theme_bg: u32,
    pub theme_fg: u32,
}

fn rgb(cr: &cairo::Context, c: u32) {
    cr.set_source_rgb(
        ((c >> 16) & 0xff) as f64 / 255.0,
        ((c >> 8) & 0xff) as f64 / 255.0,
        (c & 0xff) as f64 / 255.0,
    );
}

fn rgba(cr: &cairo::Context, c: u32, a: f64) {
    cr.set_source_rgba(
        ((c >> 16) & 0xff) as f64 / 255.0,
        ((c >> 8) & 0xff) as f64 / 255.0,
        (c & 0xff) as f64 / 255.0,
        a,
    );
}

fn fill(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64) {
    cr.rectangle(x, y, w, h);
    let _ = cr.fill();
}

fn background_of(cell: &Cell, info: &GridInfo) -> Option<u32> {
    let f = cell.flags;
    if f & FLAG_SEARCH_FOCUS != 0 {
        Some(SEARCH_FOCUS_BG)
    } else if f & FLAG_SEARCH_MATCH != 0 {
        Some(SEARCH_MATCH_BG)
    } else if f & FLAG_SELECTED != 0 {
        Some(info.selection_bg)
    } else if f & FLAG_DEFAULT_BG != 0 {
        None
    } else {
        Some(cell.bg)
    }
}

fn foreground_of(cell: &Cell, info: &GridInfo) -> u32 {
    let f = cell.flags;
    if f & (FLAG_SEARCH_FOCUS | FLAG_SEARCH_MATCH) != 0 {
        SEARCH_FG
    } else if f & FLAG_SELECTED != 0 {
        info.selection_fg
    } else {
        cell.fg
    }
}

/// Paints the whole pane.
pub fn paint(
    cr: &cairo::Context,
    fonts: &Fonts,
    snap: &Snapshot,
    p: &PaintParams<'_>,
    images: &mut ImageCache,
) {
    let info = &snap.info;
    let default_bg = if snap.valid { info.background } else { p.theme_bg };
    let default_fg = if snap.valid { info.foreground } else { p.theme_fg };
    let _ = cr.save();
    cr.set_operator(cairo::Operator::Source);
    rgba(cr, default_bg, p.opacity);
    let _ = cr.paint();
    cr.set_operator(cairo::Operator::Over);
    let _ = cr.restore();

    let cols = snap.cols();
    let rows = snap.rows();
    if snap.valid && cols > 0 && rows > 0 && snap.cells.len() >= cols * rows {
        let _ = cr.save();
        if p.scroll_y > 0.0 {
            cr.rectangle(
                0.0,
                p.pad_y,
                p.width,
                (rows as f64 * fonts.cell_h).min(p.height - p.pad_y),
            );
            cr.clip();
        }
        paint_grid(cr, fonts, snap, p, images);
        let _ = cr.restore();
    }

    if p.dim > 0.001 {
        rgba(cr, default_bg, p.dim);
        let _ = cr.paint();
    }
    if p.flash > 0.001 {
        rgba(cr, default_fg, 0.22 * p.flash);
        let _ = cr.paint();
    }
}

struct RowSrc<'a> {
    /// -1 for the peek line.
    row: i32,
    cells: &'a [Cell],
    clusters: Box<dyn Fn(u16) -> Option<String> + 'a>,
}

fn paint_grid(
    cr: &cairo::Context,
    fonts: &Fonts,
    snap: &Snapshot,
    p: &PaintParams<'_>,
    images: &mut ImageCache,
) {
    let info = &snap.info;
    let cols = snap.cols();
    let rows = snap.rows();
    let cw = fonts.cell_w;
    let ch = fonts.cell_h;
    let row_y = |r: i32| p.pad_y + r as f64 * ch + p.scroll_y;

    let mut sources: Vec<RowSrc<'_>> = Vec::with_capacity(rows + 1);
    if p.scroll_y > 0.0
        && let Some((cells, clusters)) = &snap.peek
        && cells.len() == cols
    {
        sources.push(RowSrc {
            row: -1,
            cells,
            clusters: Box::new(move |c| clusters.get(&c).cloned()),
        });
    }
    for r in 0..rows {
        let cells = &snap.cells[r * cols..(r + 1) * cols];
        sources.push(RowSrc {
            row: r as i32,
            cells,
            clusters: Box::new(move |c| snap.clusters.get(&(r as u16, c)).cloned()),
        });
    }

    // Backgrounds, merged per row into runs of one color.
    for src in &sources {
        let y = row_y(src.row);
        let mut c = 0;
        while c < cols {
            let Some(bg) = background_of(&src.cells[c], info) else {
                c += 1;
                continue;
            };
            let start = c;
            while c < cols && background_of(&src.cells[c], info) == Some(bg) {
                c += 1;
            }
            rgb(cr, bg);
            fill(cr, p.pad_x + start as f64 * cw, y, (c - start) as f64 * cw, ch);
        }
    }

    // Cursor.
    let crow = info.cursor_row as usize;
    let ccol = info.cursor_col as usize;
    let cursor_visible = info.modes & MODE_SHOW_CURSOR != 0
        && info.cursor_shape != CURSOR_HIDDEN
        && info.display_offset == 0
        && crow < rows
        && ccol < cols
        && (p.cursor_on || !p.focused)
        && p.preedit.is_none_or(|s| s.is_empty());
    let shape = if p.focused { info.cursor_shape } else { CURSOR_HOLLOW };
    let cursor_cell = snap.cell(crow, ccol).copied().unwrap_or_default();
    let cursor_color = if info.cursor_color & NO_COLOR != 0 {
        if cursor_cell.fg & NO_COLOR != 0 { info.foreground } else { foreground_of(&cursor_cell, info) }
    } else {
        info.cursor_color
    };
    let cx = p.pad_x + ccol as f64 * cw;
    let cy = row_y(crow as i32);
    let cwidth = if info.cursor_wide { 2.0 * cw } else { cw };
    let block = cursor_visible && shape == CURSOR_BLOCK;
    if block {
        rgb(cr, cursor_color);
        fill(cr, cx, cy, cwidth, ch);
    }
    let cursor_text = if info.cursor_text_color & NO_COLOR != 0 {
        info.background
    } else {
        info.cursor_text_color
    };
    let text_color = |src_row: i32, col: usize, cell: &Cell| {
        if block && src_row == crow as i32 && col == ccol {
            cursor_text
        } else {
            foreground_of(cell, info)
        }
    };

    paint_images(cr, snap, p, fonts, images, |z| z < 0);

    // Text and procedural glyphs.
    for src in &sources {
        let y = row_y(src.row);
        let baseline_y = y + fonts.baseline;
        let shaped = fonts.shape_row(src.cells, &*src.clusters);
        // Batch glyphs per (font, color).
        let mut batch: Vec<cairo::Glyph> = Vec::new();
        let mut batch_key: Option<(usize, u32)> = None;
        let flush = |batch: &mut Vec<cairo::Glyph>, key: Option<(usize, u32)>| {
            if let Some((fid, color)) = key
                && !batch.is_empty()
            {
                let table = fonts.fonts.borrow();
                cr.set_scaled_font(&table[fid].scaled);
                rgb(cr, color);
                let _ = cr.show_glyphs(batch);
            }
            batch.clear();
        };
        for g in shaped.iter() {
            let col = g.col as usize;
            if col >= cols {
                continue;
            }
            let cell = &src.cells[col];
            if cell.flags & FLAG_HIDDEN != 0 {
                continue;
            }
            let color = text_color(src.row, col, cell);
            if g.span > 0 {
                flush(&mut batch, batch_key);
                batch_key = None;
                paint_symbol(cr, fonts, g, p.pad_x + col as f64 * cw, y, color);
                continue;
            }
            let key = Some((g.font, color));
            if key != batch_key {
                flush(&mut batch, batch_key);
                batch_key = key;
            }
            batch.push(cairo::Glyph::new(
                g.glyph as u64,
                (p.pad_x + col as f64 * cw + g.x.round()).round(),
                (baseline_y + g.y.round()).round(),
            ));
        }
        flush(&mut batch, batch_key);

        // Box drawing and blocks, without antialiasing so cells tile seamlessly.
        cr.set_antialias(cairo::Antialias::None);
        for (c, cell) in src.cells.iter().enumerate() {
            if cell.flags & (FLAG_HIDDEN | FLAG_WIDE_SPACER) != 0
                || !boxdraw::is_procedural(cell.ch)
                || (src.clusters)(c as u16).is_some()
            {
                continue;
            }
            boxdraw::paint(
                cr,
                cell.ch,
                p.pad_x + c as f64 * cw,
                y,
                cw as i32,
                ch as i32,
                fonts.line as i32,
                text_color(src.row, c, cell),
            );
        }
        cr.set_antialias(cairo::Antialias::Default);

        // Underlines and strikethrough.
        for (c, cell) in src.cells.iter().enumerate() {
            if cell.flags & FLAG_WIDE_SPACER != 0 {
                continue;
            }
            let mut style = underline_style(cell.flags);
            if style == 0 && src.row >= 0 && p.hover.covers(src.row as usize, c, cell) {
                style = 1;
            }
            let strike = cell.flags & FLAG_STRIKEOUT != 0;
            if style == 0 && !strike {
                continue;
            }
            let fg = text_color(src.row, c, cell);
            let x = p.pad_x + c as f64 * cw;
            let width = if cell.flags & FLAG_WIDE != 0 { 2.0 * cw } else { cw };
            if style != 0 {
                let color = if cell.ul & NO_COLOR != 0 { fg } else { cell.ul };
                rgb(cr, color);
                underline(cr, fonts, style, x, y, width);
            }
            if strike {
                rgb(cr, fg);
                let t = fonts.line;
                let sy = (baseline_y - fonts.strike_offset - t / 2.0).round();
                fill(cr, x, sy, width, t);
            }
        }
    }

    paint_images(cr, snap, p, fonts, images, |z| z >= 0);

    if cursor_visible && !block {
        rgb(cr, cursor_color);
        let t = (p.cursor_thickness).round().max(1.0);
        match shape {
            CURSOR_BEAM => fill(cr, cx, cy, t, ch),
            CURSOR_UNDERLINE => fill(cr, cx, cy + ch - t, cwidth, t),
            _ => {
                let b = (t / 2.0).round().max(1.0);
                fill(cr, cx, cy, cwidth, b);
                fill(cr, cx, cy + ch - b, cwidth, b);
                fill(cr, cx, cy, b, ch);
                fill(cr, cx + cwidth - b, cy, b, ch);
            }
        }
    }

    if let Some(text) = p.preedit.filter(|s| !s.is_empty()) {
        paint_preedit(cr, fonts, text, cx, cy, p.theme_fg, p.theme_bg);
    }
}

/// 1 single, 2 double, 3 curly, 4 dotted, 5 dashed (curly wins over double over dotted…).
fn underline_style(flags: u16) -> u8 {
    if flags & FLAG_UNDERCURL != 0 {
        3
    } else if flags & FLAG_DOUBLE_UNDERLINE != 0 {
        2
    } else if flags & FLAG_DOTTED_UNDERLINE != 0 {
        4
    } else if flags & FLAG_DASHED_UNDERLINE != 0 {
        5
    } else if flags & FLAG_UNDERLINE != 0 {
        1
    } else {
        0
    }
}

fn underline(cr: &cairo::Context, fonts: &Fonts, style: u8, x: f64, y: f64, width: f64) {
    let t = fonts.line;
    let ch = fonts.cell_h;
    let uy = (y + fonts.baseline + fonts.underline_offset).min(y + ch - t);
    match style {
        2 => {
            let second = (uy + 2.0 * t).min(y + ch - t);
            let first = (second - 2.0 * t).max(y);
            fill(cr, x, first, width, t);
            fill(cr, x, second, width, t);
        }
        3 => {
            let h = (3.0 * t).max((0.2 * ch).min(5.0 * t));
            let top = (uy - h / 2.0 + t / 2.0).min(y + ch - h);
            let amp = (h - t) / 2.0;
            let mid = top + h / 2.0;
            cr.set_line_width(t);
            let mut px = x;
            cr.move_to(px, mid + amp * (std::f64::consts::TAU * px / fonts.cell_w).sin());
            while px < x + width {
                px = (px + 1.0).min(x + width);
                cr.line_to(px, mid + amp * (std::f64::consts::TAU * px / fonts.cell_w).sin());
            }
            let _ = cr.stroke();
        }
        4 | 5 => {
            let dash = if style == 4 { t } else { (0.4 * fonts.cell_w).max(2.0) };
            // Phase from the absolute x, so patterns continue across cells.
            let mut px = x - (x % (2.0 * dash));
            while px < x + width {
                let a = px.max(x);
                let b = (px + dash).min(x + width);
                if b > a {
                    cr.rectangle(a, uy, b - a, t);
                }
                px += 2.0 * dash;
            }
            let _ = cr.fill();
        }
        _ => fill(cr, x, uy, width, t),
    }
}

/// A Nerd Font icon scaled into its cell(s): 94% × 88% of the box, centered.
fn paint_symbol(cr: &cairo::Context, fonts: &Fonts, g: &ShapedGlyph, x: f64, y: f64, color: u32) {
    let table = fonts.fonts.borrow();
    let entry = &table[g.font];
    let (ink, _) = entry.font.glyph_extents(g.glyph);
    let s = pango::SCALE as f64;
    let (iw, ih) = (ink.width() as f64 / s, ink.height() as f64 / s);
    if iw <= 0.0 || ih <= 0.0 {
        return;
    }
    let box_w = fonts.cell_w * g.span as f64;
    let box_h = fonts.cell_h;
    let scale = (box_w * 0.94 / iw).min(box_h * 0.88 / ih);
    let (w, h) = (iw * scale, ih * scale);
    let tx = x + (box_w - w) / 2.0;
    let ty = y + (box_h - h) / 2.0;
    let _ = cr.save();
    cr.translate(tx - ink.x() as f64 / s * scale, ty - ink.y() as f64 / s * scale);
    cr.scale(scale, scale);
    cr.set_scaled_font(&entry.scaled);
    rgb(cr, color);
    let _ = cr.show_glyphs(&[cairo::Glyph::new(g.glyph as u64, 0.0, 0.0)]);
    let _ = cr.restore();
}

fn paint_images(
    cr: &cairo::Context,
    snap: &Snapshot,
    p: &PaintParams<'_>,
    fonts: &Fonts,
    images: &mut ImageCache,
    layer: impl Fn(i32) -> bool,
) {
    for pl in snap.images.iter().filter(|pl| layer(pl.z)) {
        let Some(entry) = images.get_mut(&pl.image) else { continue };
        entry.last_used = Instant::now();
        let tw = entry.surface.width() as f64;
        let th = entry.surface.height() as f64;
        let src_w = if pl.src_w > 0 { pl.src_w as f64 } else { tw };
        let src_h = if pl.src_h > 0 { pl.src_h as f64 } else { th };
        let dst_w = if pl.dst_w > 0 {
            pl.dst_w as f64
        } else if pl.cols > 0 {
            pl.cols as f64 * fonts.cell_w
        } else {
            src_w
        };
        let dst_h = if pl.dst_h > 0 {
            pl.dst_h as f64
        } else if pl.rows > 0 {
            pl.rows as f64 * fonts.cell_h
        } else {
            src_h
        };
        if src_w <= 0.0 || src_h <= 0.0 {
            continue;
        }
        let x = p.pad_x + pl.col as f64 * fonts.cell_w + pl.x_offset as f64;
        let y = p.pad_y + pl.row as f64 * fonts.cell_h + pl.y_offset as f64 + p.scroll_y;
        let _ = cr.save();
        cr.translate(x, y);
        cr.scale(dst_w / src_w, dst_h / src_h);
        let _ = cr.set_source_surface(&entry.surface, -(pl.src_x as f64), -(pl.src_y as f64));
        cr.source().set_filter(cairo::Filter::Good);
        cr.rectangle(0.0, 0.0, src_w, src_h);
        let _ = cr.fill();
        let _ = cr.restore();
    }
}

/// The input method's preedit at the cursor: theme colors, underlined.
fn paint_preedit(cr: &cairo::Context, fonts: &Fonts, text: &str, x: f64, y: f64, fg: u32, bg: u32) {
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(&fonts.descs[0]));
    layout.set_text(text);
    let (_, logical) = layout.pixel_extents();
    let w = (logical.width() as f64).max(fonts.cell_w);
    let h = (logical.height() as f64).max(fonts.cell_h);
    rgb(cr, bg);
    fill(cr, x, y, w, h);
    rgb(cr, fg);
    cr.move_to(x, y + (h - logical.height() as f64) / 2.0);
    pangocairo::functions::show_layout(cr, &layout);
    fill(cr, x, y + h - fonts.line, w, fonts.line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features() {
        assert_eq!(
            feature_settings(&["-calt".into(), "ss01".into(), "+zero".into(), "cv01=2".into(), "bad".into()]),
            "calt=0,ss01=1,zero=1,cv01=2"
        );
    }

    fn cell(ch: char) -> Cell {
        Cell {
            ch: ch as u32,
            ..Default::default()
        }
    }

    #[test]
    fn row_text_and_links() {
        let mut snap = Snapshot::default();
        snap.info.cols = 30;
        snap.info.rows = 1;
        let text = "go to https://x.org/a. ok";
        snap.cells = text.chars().map(cell).collect();
        snap.cells.resize(30, Cell::default());
        snap.valid = true;
        let (t, map) = snap.row_text(0);
        assert!(t.starts_with(text));
        assert_eq!(map[3], 3);
        let (url, hover) = snap.link_at(0, 10).unwrap();
        assert_eq!(url, "https://x.org/a");
        assert_eq!(hover, Hover::Span(0, 6, 20));
        assert!(snap.link_at(0, 2).is_none());
    }
}
