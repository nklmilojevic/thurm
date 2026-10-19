import { expect, test } from "bun:test";
import { type Notice, addNotice } from "./notices";

const notice = (pane: number, body: string, permission: number | null = null): Notice => ({
  pane,
  title: "Claude Code",
  body,
  permission,
});

test("one agent prompt leaves one notice, the one with Approve and Deny", () => {
  // As the daemon sends them: the screen, then the hook's permission prompt, then the agent's
  // own notification, while the prompt (7) is still showing.
  let list = addNotice([], notice(1, "approve Bash: cargo test"), null);
  list = addNotice(list, notice(1, "approve Bash: cargo test", 7), 7);
  list = addNotice(list, notice(1, "Claude needs your permission"), 7);
  expect(list).toEqual([notice(1, "approve Bash: cargo test", 7)]);
});

test("a newer notice replaces the pane's older one", () => {
  let list = addNotice([], notice(1, "is waiting for your input"), null);
  list = addNotice(list, notice(1, "finished after 2m"), null);
  expect(list).toEqual([notice(1, "finished after 2m")]);
});

test("once its prompt is gone, a permission notice gives way", () => {
  // Answered on the Mac: the pane shows no prompt, and the turn ends.
  let list = addNotice([], notice(1, "approve Bash: ls", 7), 7);
  list = addNotice(list, notice(1, "finished after 5s"), null);
  expect(list).toEqual([notice(1, "finished after 5s")]);
});

test("a new permission prompt replaces the old one", () => {
  let list = addNotice([], notice(1, "approve Bash: ls", 7), 7);
  list = addNotice(list, notice(1, "approve Edit: main.rs", 8), 8);
  expect(list).toEqual([notice(1, "approve Edit: main.rs", 8)]);
});

test("other panes' notices stay, newest first", () => {
  let list = addNotice([], notice(1, "approve Bash: ls", 7), 7);
  list = addNotice(list, notice(2, "finished"), null);
  list = addNotice(list, notice(3, "Build done"), null);
  expect(list.map((n) => n.pane)).toEqual([3, 2, 1]);
});
