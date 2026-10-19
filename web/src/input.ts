// Keyboard, IME, clipboard and mouse for the focused pane. Keys go to a hidden textarea at the
// cursor, so the browser's text input (layouts, dead keys, IME candidates) works as usual;
// the wasm session encodes keys like the apps do.
import type { Conn } from "./conn";
import type { PaneView } from "./render";

export const isMac = /Mac|iPhone|iPad/.test(navigator.platform);

// thurm_proto::mods
const SHIFT = 1;
const ALT = 2;
const CTRL = 4;
const SUPER = 8;
const CAPS_LOCK = 64;
const NUM_LOCK = 128;

export function mods(e: KeyboardEvent | MouseEvent): number {
  let m = 0;
  if (e.shiftKey) m |= SHIFT;
  if (e.altKey) m |= ALT;
  if (e.ctrlKey) m |= CTRL;
  if (e.metaKey && !isMac) m |= SUPER;
  if (e.getModifierState?.("CapsLock")) m |= CAPS_LOCK;
  if (e.getModifierState?.("NumLock")) m |= NUM_LOCK;
  return m;
}

export type Target = { pane: number; view: PaneView } | null;

export class Input {
  readonly area = document.getElementById("input") as HTMLTextAreaElement;
  private composing = false;
  private wheelPx = 0;

  constructor(
    private conn: Conn,
    private target: () => Target,
    private onActivity: () => void,
    private preedit: (text: string) => void,
  ) {
    const a = this.area;
    a.addEventListener("keydown", (e) => this.keydown(e));
    a.addEventListener("keyup", (e) => this.key(e, 2));
    a.addEventListener("compositionstart", () => {
      this.composing = true;
    });
    a.addEventListener("compositionupdate", (e) => this.preedit(e.data));
    a.addEventListener("compositionend", (e) => {
      this.composing = false;
      this.preedit("");
      this.text(e.data);
    });
    a.addEventListener("input", (e) => {
      const ev = e as InputEvent;
      if (this.composing || ev.isComposing) return;
      if (ev.inputType === "insertText" || ev.inputType === "insertReplacementText") this.text(ev.data ?? a.value);
      a.value = "";
    });
    a.addEventListener("paste", (e) => {
      e.preventDefault();
      const text = e.clipboardData?.getData("text/plain") ?? "";
      const t = this.target();
      if (t && text) this.conn.with((c) => c.paste(t.pane, text));
    });
    document.addEventListener("copy", (e) => {
      const t = this.target();
      const text = t ? (this.conn.client?.selection_text(t.pane) ?? "") : "";
      if (!text) return;
      e.preventDefault();
      e.clipboardData?.setData("text/plain", text);
    });
  }

  focus() {
    this.area.focus({ preventScroll: true });
  }

  /** Keeps the textarea (and so the IME's candidate window) at the cursor. */
  place(x: number, y: number, h: number) {
    this.area.style.left = `${x}px`;
    this.area.style.top = `${y}px`;
    this.area.style.height = `${h}px`;
  }

  private keydown(e: KeyboardEvent) {
    if (this.composing || e.isComposing || e.keyCode === 229) return;
    const copyCombo = isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && e.shiftKey;
    const k = e.key.toLowerCase();
    if (copyCombo && k === "c") {
      const t = this.target();
      const text = t ? this.conn.client?.selection_text(t.pane) : "";
      if (text) {
        e.preventDefault();
        void navigator.clipboard?.writeText(text).catch(() => document.execCommand("copy"));
      }
      return;
    }
    // Paste comes as a paste event; Cmd shortcuts belong to the browser on a Mac.
    if (copyCombo && k === "v") return;
    if (isMac && e.metaKey) return;
    this.key(e, e.repeat ? 1 : 0);
  }

  private key(e: KeyboardEvent, action: number) {
    const t = this.target();
    if (!t || this.composing) return;
    let m = mods(e);
    // Option composes characters on a Mac (é, ∑): the character is the key, without Alt.
    if (isMac && e.altKey && [...e.key].length === 1 && !e.ctrlKey) m &= ~ALT;
    const sent = this.conn.with((c) => c.key(t.pane, e.code, e.key, m, action));
    if (sent) {
      e.preventDefault();
      if (action !== 2) this.onActivity();
    }
  }

  private text(text: string) {
    const t = this.target();
    if (!t || !text) return;
    this.conn.with((c) => c.text(t.pane, text));
    this.area.value = "";
    this.onActivity();
  }

  /** Mouse handling on a pane's canvas. */
  attach(pane: number, view: PaneView, focus: () => void) {
    const c = view.canvas;
    const send = (e: PointerEvent, kind: number) => {
      const r = c.getBoundingClientRect();
      const x = e.clientX - r.left;
      const y = e.clientY - r.top;
      const cell = view.cellAt(x, y);
      const button = kind === 2 && e.buttons === 0 ? 255 : e.button === 0 ? 0 : e.button === 1 ? 1 : e.button === 2 ? 2 : e.button === 3 ? 3 : 4;
      this.conn.with((cl) =>
        cl.mouse(pane, kind, button, mods(e), Math.min(Math.max(e.detail, 1), 3), cell.col, cell.row, cell.rightHalf, Math.round(x), Math.round(y)),
      );
    };
    c.addEventListener("pointerdown", (e) => {
      focus();
      c.setPointerCapture(e.pointerId);
      send(e, 0);
      e.preventDefault();
      this.focus();
    });
    c.addEventListener("pointermove", (e) => send(e, 2));
    c.addEventListener("pointerup", (e) => send(e, 1));
    c.addEventListener("contextmenu", (e) => e.preventDefault());
    c.addEventListener(
      "wheel",
      (e) => {
        e.preventDefault();
        const lineCss = view.metrics.ch / view.metrics.dpr;
        this.wheelPx += e.deltaMode === 1 ? e.deltaY * lineCss : e.deltaMode === 2 ? e.deltaY * lineCss * view.rows : e.deltaY;
        const lines = Math.trunc(this.wheelPx / lineCss);
        if (lines === 0) return;
        this.wheelPx -= lines * lineCss;
        const r = c.getBoundingClientRect();
        const cell = view.cellAt(e.clientX - r.left, e.clientY - r.top);
        // Positive is up, toward history.
        this.conn.with((cl) => cl.wheel(pane, -lines, cell.col, cell.row, mods(e)));
      },
      { passive: false },
    );
  }
}
