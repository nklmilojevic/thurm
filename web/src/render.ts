// Canvas2D drawing of a pane from the rows the session hands out (thurm_web::grid): only the
// rows that changed are repainted. Colors arrive resolved (0x00RRGGBB).

export const CELL_WORDS = 5;
const NO_COLOR = 0xff000000;

// thurm_proto::cell_flags
const BOLD = 1 << 0;
const ITALIC = 1 << 1;
const UNDERLINE = 1 << 2;
const DOUBLE_UNDERLINE = 1 << 3;
const UNDERCURL = 1 << 4;
const DOTTED_UNDERLINE = 1 << 5;
const DASHED_UNDERLINE = 1 << 6;
const STRIKEOUT = 1 << 7;
const WIDE = 1 << 8;
const WIDE_SPACER = 1 << 9;
const HIDDEN = 1 << 10;
const SELECTED = 1 << 11;
const DEFAULT_BG = 1 << 13;
const DIM = 1 << 14;

export type Colors = {
  foreground: number;
  background: number;
  cursor: number;
  cursor_text: number;
  selection_fg: number;
  selection_bg: number;
};

export type DrawMeta = {
  cols: number;
  rows: number;
  cursor: { col: number; row: number; shape: string; blinking: boolean; wide: boolean };
  display_offset: number;
  history_size: number;
  modes: number;
  colors: Colors;
  title: string;
  dirty: number[];
  clusters: [number, number, string][];
  links: string[];
};

/** Cell geometry in device pixels, from the font. */
export type Metrics = {
  family: string;
  size: number;
  dpr: number;
  cw: number;
  ch: number;
  baseline: number;
};

export async function measure(family: string, size: number): Promise<Metrics> {
  const dpr = window.devicePixelRatio || 1;
  const px = size * dpr;
  try {
    await document.fonts.load(`${px}px ${family}`);
  } catch {
    // An installed or missing family: measure what the browser falls back to.
  }
  const ctx = document.createElement("canvas").getContext("2d")!;
  ctx.font = `${px}px ${family}`;
  const m = ctx.measureText("M");
  const ascent = m.fontBoundingBoxAscent || px * 0.8;
  const descent = m.fontBoundingBoxDescent || px * 0.2;
  return {
    family,
    size,
    dpr,
    cw: Math.max(1, Math.round(m.width)),
    ch: Math.max(1, Math.ceil(ascent + descent)),
    baseline: Math.round(ascent),
  };
}

const css = (rgb: number) => `#${(rgb & 0xffffff).toString(16).padStart(6, "0")}`;

export class PaneView {
  readonly canvas = document.createElement("canvas");
  private ctx = this.canvas.getContext("2d", { alpha: false })!;
  cols = 0;
  rows = 0;
  /** The text of each row, for selection-free reading (tests, accessibility). */
  text: string[] = [];
  meta: DrawMeta | null = null;
  focused = false;

  constructor(public metrics: Metrics) {}

  /** Sizes the canvas for cols×rows; true when it changed (and everything must be redrawn). */
  size(cols: number, rows: number): boolean {
    const { cw, ch, dpr } = this.metrics;
    const w = cols * cw;
    const h = rows * ch;
    if (this.canvas.width === w && this.canvas.height === h) return false;
    this.canvas.width = w;
    this.canvas.height = h;
    this.canvas.style.width = `${w / dpr}px`;
    this.canvas.style.height = `${h / dpr}px`;
    return true;
  }

  paint(meta: DrawMeta, cells: Uint32Array) {
    if (meta.cols !== this.cols || meta.rows !== this.rows) {
      this.cols = meta.cols;
      this.rows = meta.rows;
      this.text = Array.from({ length: meta.rows }, () => "");
    }
    this.size(meta.cols, meta.rows);
    this.meta = meta;
    const clusters = new Map<number, string>();
    for (const [row, col, text] of meta.clusters) clusters.set(row * 65536 + col, text);
    meta.dirty.forEach((row, i) => {
      this.paintRow(meta, row, cells.subarray(i * meta.cols * CELL_WORDS, (i + 1) * meta.cols * CELL_WORDS), clusters);
    });
  }

  private paintRow(meta: DrawMeta, row: number, cells: Uint32Array, clusters: Map<number, string>) {
    const { ctx } = this;
    const { cw, ch, baseline, family, size, dpr } = this.metrics;
    const colors = meta.colors;
    const y = row * ch;
    ctx.fillStyle = css(colors.background);
    ctx.fillRect(0, y, this.cols * cw, ch);

    const cursor = meta.cursor;
    const cursorHere = cursor.row === row && cursor.shape !== "Hidden";
    let line = "";
    for (let col = 0; col < this.cols; col++) {
      const o = col * CELL_WORDS;
      const flags = cells[o + 4] & 0xffff;
      let fg = cells[o + 1];
      let bg = cells[o + 2];
      if (flags & SELECTED) {
        bg = colors.selection_bg;
        if (colors.selection_fg !== NO_COLOR) fg = colors.selection_fg;
      }
      const block = cursorHere && cursor.col === col && cursor.shape === "Block" && this.focused;
      if (block) {
        bg = colors.cursor;
        fg = colors.cursor_text;
      }
      const width = flags & WIDE ? 2 : 1;
      if (block || flags & SELECTED || !(flags & DEFAULT_BG)) {
        ctx.fillStyle = css(bg);
        ctx.fillRect(col * cw, y, cw * width, ch);
      }
      if (flags & WIDE_SPACER) continue;
      const code = cells[o];
      const glyph = clusters.get(row * 65536 + col) ?? (code > 32 ? String.fromCodePoint(code) : " ");
      line += glyph;
      if (glyph === " " || flags & HIDDEN) continue;
      ctx.font = `${flags & ITALIC ? "italic " : ""}${flags & BOLD ? "bold " : ""}${size * dpr}px ${family}`;
      ctx.fillStyle = css(fg);
      ctx.globalAlpha = flags & DIM ? 0.6 : 1;
      ctx.fillText(glyph, col * cw, y + baseline);
      ctx.globalAlpha = 1;
      const decorations = flags & (UNDERLINE | DOUBLE_UNDERLINE | UNDERCURL | DOTTED_UNDERLINE | DASHED_UNDERLINE | STRIKEOUT);
      if (decorations) this.decorate(flags, cells[o + 3] === NO_COLOR ? fg : cells[o + 3], fg, col * cw, y, cw * width);
    }
    this.text[row] = line.trimEnd();

    if (cursorHere && !(cursor.shape === "Block" && this.focused)) {
      const x = cursor.col * cw;
      const w = cursor.wide ? cw * 2 : cw;
      const t = Math.max(1, Math.round(dpr * (cursor.shape === "Beam" ? 2 : 1.5)));
      ctx.fillStyle = css(colors.cursor);
      ctx.strokeStyle = css(colors.cursor);
      ctx.lineWidth = t;
      if (cursor.shape === "Beam") ctx.fillRect(x, y, t, ch);
      else if (cursor.shape === "Underline") ctx.fillRect(x, y + ch - t, w, t);
      else ctx.strokeRect(x + t / 2, y + t / 2, w - t, ch - t);
    }
  }

  private decorate(flags: number, ul: number, fg: number, x: number, y: number, w: number) {
    const { ctx } = this;
    const { ch, dpr } = this.metrics;
    const t = Math.max(1, Math.round(dpr));
    const under = y + ch - t * 2;
    ctx.strokeStyle = css(ul);
    ctx.lineWidth = t;
    ctx.beginPath();
    if (flags & UNDERCURL) {
      const amp = t * 1.5;
      ctx.moveTo(x, under);
      for (let i = 0; i <= 4; i++) ctx.lineTo(x + (w * i) / 4, under + (i % 2 ? amp : -amp) / 2);
    } else if (flags & (UNDERLINE | DOUBLE_UNDERLINE | DOTTED_UNDERLINE | DASHED_UNDERLINE)) {
      ctx.setLineDash(flags & DOTTED_UNDERLINE ? [t, t] : flags & DASHED_UNDERLINE ? [t * 3, t * 2] : []);
      ctx.moveTo(x, under + t / 2);
      ctx.lineTo(x + w, under + t / 2);
      if (flags & DOUBLE_UNDERLINE) {
        ctx.moveTo(x, under - t * 1.5);
        ctx.lineTo(x + w, under - t * 1.5);
      }
    }
    ctx.stroke();
    ctx.setLineDash([]);
    if (flags & STRIKEOUT) {
      ctx.strokeStyle = css(fg);
      ctx.beginPath();
      ctx.moveTo(x, y + ch / 2);
      ctx.lineTo(x + w, y + ch / 2);
      ctx.stroke();
    }
  }

  /** The cell under a point in CSS pixels relative to the canvas. */
  cellAt(x: number, y: number): { col: number; row: number; rightHalf: boolean } {
    const { cw, ch, dpr } = this.metrics;
    const px = x * dpr;
    const col = Math.min(Math.max(Math.floor(px / cw), 0), Math.max(this.cols - 1, 0));
    const row = Math.min(Math.max(Math.floor((y * dpr) / ch), 0), Math.max(this.rows - 1, 0));
    return { col, row, rightHalf: px - col * cw > cw / 2 };
  }

  /** Where the cursor is, in CSS pixels relative to the canvas (for the IME). */
  cursorRect(): { x: number; y: number; h: number } {
    const { cw, ch, dpr } = this.metrics;
    const c = this.meta?.cursor ?? { col: 0, row: 0 };
    return { x: (c.col * cw) / dpr, y: (c.row * ch) / dpr, h: ch / dpr };
  }
}
