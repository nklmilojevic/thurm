// Thurm in the browser (specs/web.md): the daemon's layout, panes drawn from local terminal
// copies, typing, scrollback and agent notices.
import { Conn, type Hello, type Incoming } from "./conn";
import { Input } from "./input";
import { type Rect, type Tab, leaves, place, tabsOf } from "./layout";
import { type Notice, addNotice } from "./notices";
import { PaneView, measure, type DrawMeta, type Metrics } from "./render";

declare const __CLOCK_URL__: string;

type PaneInfo = { id: number; title: string; agent?: { status: string; name: string; permission?: number | null } | null };

const $ = (id: string) => document.getElementById(id)!;
const panesEl = $("panes");
const FONT_FAMILY = '"JetBrains Mono", ui-monospace, monospace';

const prefs = (() => {
  try {
    return {
      family: localStorage.getItem("thurm.font.family") || FONT_FAMILY,
      size: Number(localStorage.getItem("thurm.font.size")) || 13,
    };
  } catch {
    return { family: FONT_FAMILY, size: 13 };
  }
})();

let metrics: Metrics;
let tabs: Tab[] = [];
let selected = 0;
let focusedPane: number | null = null;
const views = new Map<number, { view: PaneView; el: HTMLElement; cols: number; rows: number }>();
const infos = new Map<number, PaneInfo>();
let notices: Notice[] = [];
const dirty = new Set<number>();
let frameQueued = false;
let connectedOnce = false;
let compatible = true;
let sessions = 0;

const conn = new Conn(incoming, (state, detail) => {
  if (state === "connected") sessions++;
  const s = $("status");
  s.textContent = state === "connected" ? "connected" : state === "connecting" ? "connecting" : `offline${detail ? ` (${detail})` : ""}`;
  s.className = `status ${state === "connected" ? "ok" : state === "closed" ? "error" : ""}`;
  if (state === "closed") {
    // A new session subscribes again, and focuses again.
    for (const [pane, v] of views) {
      v.el.remove();
      views.delete(pane);
    }
    focusedPane = null;
  }
});

const input = new Input(
  conn,
  () => {
    const v = focusedPane === null ? undefined : views.get(focusedPane);
    return v && focusedPane !== null ? { pane: focusedPane, view: v.view } : null;
  },
  () => {},
  (text) => showPreedit(text),
);

function banner(text: string | null) {
  const b = $("banner");
  b.hidden = !text;
  b.textContent = text ?? "";
}

function incoming(msg: Incoming) {
  switch (msg.kind) {
    case "hello":
      return hello(msg);
    case "frame":
      dirty.add(msg.pane);
      return queueFrame();
    case "event":
      return event(msg.event);
    case "error":
      return banner(msg.message);
  }
}

async function hello(h: Hello) {
  compatible = !!h.compatible && !h.error;
  if (!compatible) {
    banner(
      h.error
        ? `The daemon refused this page: ${h.error}`
        : h.newer
          ? "The daemon is newer than this page: reload it."
          : "The daemon is older than this page: update Thurm on this host.",
    );
    return;
  }
  banner(null);
  if (connectedOnce && (await newBundle())) {
    location.reload();
    return;
  }
  connectedOnce = true;
  conn
    .request("Whoami")
    .then((r: any) => {
      const p = r?.Whoami;
      $("whoami").textContent = p ? (p.login_name ?? p.display) + (p.node ? ` · ${p.node}` : "") : "";
    })
    .catch(() => {});
  await loadPanes();
  await loadLayout();
}

/** After a reconnect: the node may serve a newer bundle (the daemon was upgraded). */
async function newBundle(): Promise<boolean> {
  try {
    const html = await (await fetch("/", { cache: "no-store" })).text();
    const ours = document.querySelector<HTMLScriptElement>("script[type=module]")?.getAttribute("src");
    return !!ours && !html.includes(ours);
  } catch {
    return false;
  }
}

async function loadPanes() {
  try {
    const r = (await conn.request("ListPanes")) as { Panes?: PaneInfo[] };
    infos.clear();
    for (const p of r.Panes ?? []) infos.set(p.id, p);
  } catch {
    // Shown as a closed connection.
  }
}

async function loadLayout() {
  let next: Tab[] = [];
  try {
    const r = (await conn.request("GetLayout")) as { Layout?: string | null };
    if (r.Layout) next = tabsOf(JSON.parse(r.Layout));
  } catch {
    // No saved layout: one tab per pane.
  }
  if (next.length === 0) {
    next = [...infos.values()].map((p) => ({ workspace: "Panes", root: { type: "pane", id: p.id }, focused: p.id }));
  }
  const current = tabs[selected];
  tabs = next;
  const keep = current ? tabs.findIndex((t) => t.root && leaves(t.root).includes(current.focused)) : -1;
  selected = keep >= 0 ? keep : Math.min(selected, Math.max(tabs.length - 1, 0));
  renderTabs();
  showTab();
}

const STATUS_RANK: Record<string, number> = { NeedsInput: 4, Error: 3, Working: 2, Done: 1, Idle: 0 };
const statusClass = (s: string) => s.replace(/[A-Z]/g, (c, i) => (i ? "-" : "") + c.toLowerCase());

function tabStatus(t: Tab): string | null {
  let best: string | null = null;
  for (const id of leaves(t.root)) {
    const s = infos.get(id)?.agent?.status;
    if (s && (best === null || STATUS_RANK[s] > STATUS_RANK[best])) best = s;
  }
  return best;
}

function tabTitle(t: Tab): string {
  return t.title || infos.get(t.focused)?.title || infos.get(leaves(t.root)[0])?.title || "Terminal";
}

function renderTabs() {
  const nav = $("tabs");
  nav.replaceChildren();
  let workspace = "";
  tabs.forEach((t, i) => {
    if (t.workspace !== workspace) {
      workspace = t.workspace;
      const h = document.createElement("div");
      h.className = "workspace";
      h.textContent = workspace;
      nav.append(h);
    }
    const b = document.createElement("button");
    b.type = "button";
    b.className = `tab${i === selected ? " selected" : ""}`;
    const status = tabStatus(t);
    const dot = document.createElement("span");
    dot.className = `agent ${status ? statusClass(status) : ""}`;
    const title = document.createElement("span");
    title.className = "title";
    title.textContent = tabTitle(t);
    b.append(dot, title);
    b.onclick = () => {
      selected = i;
      renderTabs();
      showTab();
    };
    nav.append(b);
  });
}

/** Lays the selected tab out, subscribing its panes and dropping the others'. */
function showTab() {
  const tab = tabs[selected];
  const rects = new Map<number, Rect>();
  const remote: Rect[] = [];
  const box = panesEl.getBoundingClientRect();
  if (tab) place(tab.root, { x: 0, y: 0, w: box.width, h: box.height }, rects, remote);
  panesEl.querySelectorAll(".remote").forEach((e) => e.remove());
  for (const r of remote) {
    const el = document.createElement("div");
    el.className = "pane remote";
    Object.assign(el.style, { left: `${r.x}px`, top: `${r.y}px`, width: `${r.w}px`, height: `${r.h}px` });
    el.textContent = "A pane on another host: open it from that host's page.";
    panesEl.append(el);
  }
  for (const [pane, v] of views) {
    if (!rects.has(pane)) {
      conn.with((c) => c.unsubscribe(pane));
      v.el.remove();
      views.delete(pane);
    }
  }
  const { cw, ch, dpr } = metrics;
  for (const [pane, r] of rects) {
    const cols = Math.max(2, Math.floor((r.w * dpr) / cw));
    const rows = Math.max(1, Math.floor((r.h * dpr) / ch));
    let v = views.get(pane);
    if (!v) {
      const view = new PaneView(metrics);
      const el = document.createElement("div");
      el.className = "pane";
      el.dataset.pane = String(pane);
      el.append(view.canvas);
      panesEl.append(el);
      input.attach(pane, view, () => focusPane(pane));
      v = { view, el, cols: 0, rows: 0 };
      views.set(pane, v);
      conn.with((c) => c.subscribe(pane, cols, rows, Math.round(cw / dpr), Math.round(ch / dpr)));
    } else if (v.cols !== cols || v.rows !== rows) {
      conn.with((c) => c.resize(pane, cols, rows, Math.round(cw / dpr), Math.round(ch / dpr)));
    }
    v.cols = cols;
    v.rows = rows;
    Object.assign(v.el.style, { left: `${r.x}px`, top: `${r.y}px`, width: `${r.w}px`, height: `${r.h}px` });
  }
  const want = tab && rects.has(tab.focused) ? tab.focused : [...rects.keys()][0];
  if (want !== undefined && want !== focusedPane) focusPane(want);
  if (want === undefined) focusedPane = null;
}

function focusPane(pane: number) {
  if (focusedPane === pane) return;
  const had = focusedPane;
  focusedPane = pane;
  const visible = document.visibilityState === "visible" && document.hasFocus();
  if (had !== null && views.has(had)) {
    views.get(had)!.view.focused = false;
    views.get(had)!.el.classList.remove("focused");
    conn.with((c) => c.focus(had, false));
  }
  const v = views.get(pane);
  if (v) {
    v.view.focused = visible;
    v.el.classList.add("focused");
    conn.with((c) => c.focus(pane, visible));
    document.title = infos.get(pane)?.title || "Thurm";
  }
  input.focus();
}

/** The page gained or lost the user: the focused pane follows (and is read when it's back). */
function syncFocus() {
  if (focusedPane === null) return;
  const visible = document.visibilityState === "visible" && document.hasFocus();
  const v = views.get(focusedPane);
  if (!v || v.view.focused === visible) return;
  v.view.focused = visible;
  const pane = focusedPane;
  conn.with((c) => c.focus(pane, visible));
}

function queueFrame() {
  if (frameQueued) return;
  frameQueued = true;
  requestAnimationFrame(() => {
    frameQueued = false;
    const again: number[] = [];
    for (const pane of dirty) {
      const v = views.get(pane);
      const out = v && conn.client?.draw(pane);
      if (!v || !out) continue;
      const meta = JSON.parse(out.meta) as DrawMeta;
      const cells = out.take_cells();
      out.free();
      if (v.view.size(meta.cols, meta.rows) && meta.dirty.length < meta.rows) {
        // A new canvas is blank: everything again.
        conn.client?.invalidate(pane);
        again.push(pane);
        continue;
      }
      v.view.paint(meta, cells);
      if (pane === focusedPane) {
        const c = v.view.cursorRect();
        const box = v.el.getBoundingClientRect();
        input.place(box.left + c.x, box.top + c.y, c.h);
        if (meta.title) document.title = meta.title;
      }
    }
    dirty.clear();
    for (const pane of again) dirty.add(pane);
    if (dirty.size) queueFrame();
  });
}

function showPreedit(text: string) {
  let el = document.getElementById("preedit");
  if (!text) {
    el?.remove();
    return;
  }
  if (!el) {
    el = document.createElement("span");
    el.id = "preedit";
    Object.assign(el.style, {
      position: "fixed",
      zIndex: "2",
      textDecoration: "underline",
      background: "var(--bg)",
      font: `${metrics.size}px ${metrics.family}`,
    });
    document.body.append(el);
  }
  const a = input.area.getBoundingClientRect();
  Object.assign(el.style, { left: `${a.left}px`, top: `${a.top}px` });
  el.textContent = text;
}

function event(ev: Record<string, any> | string) {
  if (ev === "LayoutChanged") {
    void loadLayout();
    return;
  }
  if (typeof ev !== "object") return;
  if (ev.PaneInfo) {
    const p = ev.PaneInfo as PaneInfo;
    const known = infos.has(p.id);
    infos.set(p.id, p);
    // A pane new to this page: with no saved layout (no desktop app) it is a tab of its own.
    if (!known && !tabs.some((t) => leaves(t.root).includes(p.id))) void loadLayout();
    else renderTabs();
  } else if (ev.PaneClosed) {
    infos.delete(ev.PaneClosed.pane);
    void loadLayout();
  } else if (ev.Notify) {
    const n = ev.Notify;
    const pending = infos.get(n.pane)?.agent?.permission ?? null;
    notices = addNotice(notices, { pane: n.pane, title: n.title, body: n.body, permission: n.permission ?? null }, pending);
    renderNotices();
    if (document.visibilityState !== "visible" && "Notification" in window && Notification.permission === "granted") {
      new Notification(n.title, { body: n.body, tag: `thurm-${n.pane}` });
    }
  } else if (ev.PaneRead) {
    notices = notices.filter((x) => x.pane !== ev.PaneRead.pane);
    renderNotices();
  } else if (ev.ClipboardStore) {
    // OSC 52 copy, allowed by the host's osc52 setting before it got here.
    void navigator.clipboard?.writeText(ev.ClipboardStore.text).catch(() => banner("A program copied text, but the browser refused it without a click."));
  } else if (ev.ClipboardRequest) {
    // A page never hands the clipboard to a program unasked.
    void conn.request({ ClipboardReply: { pane: ev.ClipboardRequest.pane, text: "" } }).catch(() => {});
  }
}

function renderNotices() {
  const box = $("notices");
  box.replaceChildren();
  for (const n of notices) {
    const el = document.createElement("div");
    el.className = "notice";
    const title = document.createElement("div");
    title.className = "title";
    title.textContent = n.title;
    const body = document.createElement("div");
    body.className = "body";
    body.textContent = n.body;
    el.append(title, body);
    if (n.permission !== null) {
      const actions = document.createElement("div");
      actions.className = "actions";
      for (const allow of [true, false]) {
        const b = document.createElement("button");
        b.type = "button";
        b.className = allow ? "allow" : "deny";
        b.textContent = allow ? "Approve" : "Deny";
        b.onclick = (e) => {
          e.stopPropagation();
          conn
            .request({ AnswerPermission: { pane: n.pane, prompt: n.permission, allow } })
            .then(() => {
              notices = notices.filter((x) => x !== n);
              renderNotices();
            })
            .catch((err: Error) => {
              n.error = err.message;
              renderNotices();
            });
        };
        actions.append(b);
      }
      el.append(actions);
    }
    if (n.error) {
      const err = document.createElement("div");
      err.className = "error";
      err.textContent = n.error;
      el.append(err);
    }
    el.onclick = () => {
      const i = tabs.findIndex((t) => leaves(t.root).includes(n.pane));
      if (i >= 0) {
        selected = i;
        tabs[i].focused = n.pane;
        renderTabs();
        showTab();
        focusPane(n.pane);
      }
    };
    box.append(el);
  }
  if ("Notification" in window && Notification.permission === "default" && notices.length) {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = "Notify me when this tab is hidden";
    b.onclick = () => void Notification.requestPermission().then(renderNotices);
    box.append(b);
  }
}

async function main() {
  await Conn.load();
  metrics = await measure(prefs.family, prefs.size);
  const clock = new Worker(__CLOCK_URL__, { type: "module" });
  clock.onmessage = () => conn.tick();
  clock.postMessage(5000);
  $("disconnect").onclick = () => {
    if (conn.open) {
      conn.disconnect();
      $("disconnect").textContent = "Connect";
    } else {
      conn.connect();
      $("disconnect").textContent = "Disconnect";
    }
  };
  // In fullscreen, Keyboard Lock (Chromium) gives the terminal Ctrl+W, Ctrl+T and the like.
  $("fullscreen").onclick = () => {
    if (document.fullscreenElement) void document.exitFullscreen();
    else void document.documentElement.requestFullscreen().catch(() => {});
  };
  document.addEventListener("fullscreenchange", () => {
    const keyboard = (navigator as any).keyboard;
    if (document.fullscreenElement) void keyboard?.lock?.().catch(() => {});
    else keyboard?.unlock?.();
    input.focus();
  });
  addEventListener("resize", () => {
    if (compatible) showTab();
  });
  addEventListener("focus", syncFocus);
  addEventListener("blur", syncFocus);
  document.addEventListener("visibilitychange", () => {
    syncFocus();
    if (document.visibilityState === "visible" && !conn.open) conn.connect();
  });
  // Ctrl+W (delete a word in a shell) would close the tab.
  addEventListener("beforeunload", (e) => {
    if (conn.open) e.preventDefault();
  });
  panesEl.addEventListener("pointerdown", () => input.focus());
  conn.connect();
}

// For the end-to-end tests: what the page shows, without reading pixels.
(window as any).__thurmTest = {
  text: (pane?: number) => views.get(pane ?? focusedPane ?? -1)?.view.text.join("\n") ?? "",
  focusedPane: () => focusedPane,
  panes: () => [...views.keys()],
  notices: () => notices.map((n) => ({ ...n })),
  connected: () => conn.open,
  sessions: () => sessions,
  selection: () => (focusedPane === null ? "" : (conn.client?.selection_text(focusedPane) ?? "")),
  cell: () => ({ w: metrics.cw / metrics.dpr, h: metrics.ch / metrics.dpr }),
};

void main();
