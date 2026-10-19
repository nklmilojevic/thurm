import { expect, test } from "bun:test";
import { type LayoutNode, type Rect, leaves, place, tabsOf } from "./layout";

const pane = (id: number, host?: string): LayoutNode => ({ type: "pane", id, ...(host ? { host } : {}) });
const split = (dir: "right" | "down" | "left" | "up", ratio: number, first: LayoutNode, second: LayoutNode): LayoutNode => ({
  type: "split",
  dir,
  ratio,
  first,
  second,
});

function layout(root: LayoutNode) {
  const out = new Map<number, Rect>();
  const remote: Rect[] = [];
  place(root, { x: 0, y: 0, w: 1000, h: 600 }, out, remote);
  return { out, remote };
}

test("right and down put first on the left and top, with ratio its share", () => {
  const right = layout(split("right", 0.3, pane(1), pane(2))).out;
  expect(right.get(1)).toEqual({ x: 0, y: 0, w: 300, h: 600 });
  expect(right.get(2)).toEqual({ x: 300, y: 0, w: 700, h: 600 });
  const down = layout(split("down", 0.25, pane(1), pane(2))).out;
  expect(down.get(1)).toEqual({ x: 0, y: 0, w: 1000, h: 150 });
  expect(down.get(2)).toEqual({ x: 0, y: 150, w: 1000, h: 450 });
});

test("left and up put second on the left and top, first keeping its share", () => {
  const left = layout(split("left", 0.3, pane(1), pane(2))).out;
  expect(left.get(2)).toEqual({ x: 0, y: 0, w: 700, h: 600 });
  expect(left.get(1)).toEqual({ x: 700, y: 0, w: 300, h: 600 });
  const up = layout(split("up", 0.25, pane(1), pane(2))).out;
  expect(up.get(2)).toEqual({ x: 0, y: 0, w: 1000, h: 450 });
  expect(up.get(1)).toEqual({ x: 0, y: 450, w: 1000, h: 150 });
});

test("nested splits and remote panes", () => {
  // 1 | (2 over 3), where 2 was split up out of 3; 4 is another host's.
  const { out, remote } = layout(split("right", 0.5, pane(1), split("up", 0.5, pane(3), split("down", 0.5, pane(2), pane(4, "devbox")))));
  expect(out.get(1)).toEqual({ x: 0, y: 0, w: 500, h: 600 });
  expect(out.get(3)).toEqual({ x: 500, y: 300, w: 500, h: 300 });
  expect(out.get(2)).toEqual({ x: 500, y: 0, w: 500, h: 150 });
  expect(remote).toEqual([{ x: 500, y: 150, w: 500, h: 150 }]);
});

test("ratios out of range are clamped as in the Mac app", () => {
  expect(layout(split("right", 0, pane(1), pane(2))).out.get(1)!.w).toBe(50);
  expect(layout(split("left", 1, pane(1), pane(2))).out.get(1)!.w).toBe(950);
});

test("tabs of windows, other workspaces and the quick terminal, each once", () => {
  const tab = (id: number) => ({ root: pane(id), focused: id });
  const tabs = tabsOf({
    windows: [{ workspace: 7, tabs: [tab(1), tab(2)] }],
    workspaces: [
      { id: 7, name: "Work", tabs: [tab(1), tab(2)] },
      { id: 8, name: "", tabs: [tab(3), { root: pane(9, "devbox"), focused: 9 }] },
    ],
    quick: tab(5),
  });
  expect(tabs.map((t) => [t.workspace, t.focused])).toEqual([
    ["Work", 1],
    ["Work", 2],
    ["Workspace", 3],
    ["Quick Terminal", 5],
  ]);
  expect(leaves(split("right", 0.5, pane(1), pane(2, "devbox")))).toEqual([1]);
});
