// The daemon's saved layout (thurm_proto::Layout, JSON) as the page shows it: tabs, and where
// each of a tab's panes goes.

export type LayoutNode =
  | { type: "pane"; id: number; host?: string }
  | { type: "split"; dir: "right" | "down" | "left" | "up"; ratio: number; first: LayoutNode; second: LayoutNode };
export type Tab = { workspace: string; title?: string; root: LayoutNode; focused: number };
export type Rect = { x: number; y: number; w: number; h: number };

/** Every tab of every window and workspace, and the quick terminal's, each once. */
export function tabsOf(layout: any): Tab[] {
  const names = new Map<number, string>();
  for (const w of layout.workspaces ?? []) names.set(w.id, w.name || "Workspace");
  const out: Tab[] = [];
  const seen = new Set<string>();
  const add = (workspace: string, t: any) => {
    const key = JSON.stringify(t.root);
    if (seen.has(key)) return;
    seen.add(key);
    out.push({ workspace, title: t.title ?? undefined, root: t.root, focused: t.focused });
  };
  for (const w of layout.windows ?? []) for (const t of w.tabs ?? []) add(names.get(w.workspace) ?? "Window", t);
  for (const w of layout.workspaces ?? []) for (const t of w.tabs ?? []) add(w.name || "Workspace", t);
  if (layout.quick) add("Quick Terminal", layout.quick);
  return out.filter((t) => leaves(t.root).length > 0);
}

/** This daemon's panes in a split tree (a remote host's are someone else's). */
export function leaves(n: LayoutNode): number[] {
  if (n.type === "pane") return n.host ? [] : [n.id];
  return [...leaves(n.first), ...leaves(n.second)];
}

/**
 * Where each pane of a split tree goes in `r`; a remote host's panes go to `remote`. As in the
 * Mac app (SplitView's `init(layout:)`): `ratio` is `first`'s share, and `left`/`up` put
 * `second` on the left/top, `first` on the right/bottom.
 */
export function place(n: LayoutNode, r: Rect, out: Map<number, Rect>, remote: Rect[]) {
  if (n.type === "pane") {
    if (n.host) remote.push(r);
    else out.set(n.id, r);
    return;
  }
  const horizontal = n.dir === "right" || n.dir === "left";
  const firstShare = Math.min(Math.max(n.ratio, 0.05), 0.95);
  const before = n.dir === "left" || n.dir === "up" ? n.second : n.first;
  const after = before === n.first ? n.second : n.first;
  const share = before === n.first ? firstShare : 1 - firstShare;
  const [a, b] = horizontal
    ? [
        { ...r, w: Math.round(r.w * share) },
        { ...r, x: r.x + Math.round(r.w * share), w: r.w - Math.round(r.w * share) },
      ]
    : [
        { ...r, h: Math.round(r.h * share) },
        { ...r, y: r.y + Math.round(r.h * share), h: r.h - Math.round(r.h * share) },
      ];
  place(before, a, out, remote);
  place(after, b, out, remote);
}
