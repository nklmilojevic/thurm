// The notice list: what needs you, one notice per pane. One agent prompt reaches the page as
// several notices (the screen shows it, the hook adds its permission prompt, the agent's own
// notification follows), each newer than the last; only the newest is news. A notice that can
// be answered stays until its prompt is gone, so a later one without Approve / Deny can't hide it.

export type Notice = { pane: number; title: string; body: string; permission: number | null; error?: string };

/** `notices` with `n` added, newest first. `pending` is the permission prompt the pane shows
 * now (its `PaneInfo`), if any. */
export function addNotice(notices: Notice[], n: Notice, pending: number | null): Notice[] {
  const kept = notices.find((x) => x.pane === n.pane && x.permission !== null && x.permission === pending);
  if (n.permission === null && kept) return notices;
  return [n, ...notices.filter((x) => x.pane !== n.pane)];
}
