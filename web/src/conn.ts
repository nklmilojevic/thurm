// The WebSocket to the node's /v1/connect and the wasm session on it. A new session per
// connection: Attach restores every pane's state, so reconnecting is starting over.
import init, { Client } from "../pkg/thurm_web.js";

declare const __WASM_URL__: string;

export type Hello = {
  kind: "hello";
  version?: number;
  build?: string;
  compatible?: boolean;
  newer?: boolean;
  error?: string;
};
export type Incoming =
  | Hello
  | { kind: "response"; id: number; ok?: unknown; error?: string }
  | { kind: "event"; event: Record<string, any> | string }
  | { kind: "frame"; pane: number }
  | { kind: "error"; message: string };

export type ConnState = "connecting" | "connected" | "closed";

export class Conn {
  client: Client | null = null;
  private ws: WebSocket | null = null;
  private pending = new Map<number, (ok: unknown, error?: string) => void>();
  private backoff = 1000;
  private stopped = false;
  private retry: ReturnType<typeof setTimeout> | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private onIncoming: (msg: Incoming) => void,
    private onState: (state: ConnState, detail?: string) => void,
  ) {}

  static async load() {
    await init({ module_or_path: __WASM_URL__ });
  }

  url(): string {
    const scheme = location.protocol === "https:" ? "wss:" : "ws:";
    return `${scheme}//${location.host}/v1/connect`;
  }

  connect() {
    this.stopped = false;
    clearTimeout(this.retry);
    this.onState("connecting");
    const ws = new WebSocket(this.url());
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = () => {
      this.backoff = 1000;
      this.client?.free();
      // The terminal's default theme on the page's side of light and dark.
      this.client = new Client(matchMedia("(prefers-color-scheme: dark)").matches);
      this.onState("connected");
      this.pump();
    };
    ws.onmessage = (ev) => {
      if (!this.client || this.ws !== ws) return;
      try {
        this.client.receive(new Uint8Array(ev.data as ArrayBuffer));
      } catch (e) {
        this.onIncoming({ kind: "error", message: String(e) });
        ws.close();
        return;
      }
      this.pump();
    };
    ws.onclose = (ev) => {
      if (this.ws !== ws) return;
      this.ws = null;
      for (const done of this.pending.values()) done(undefined, "disconnected");
      this.pending.clear();
      this.onState("closed", ev.reason || (ev.code === 1006 ? "the connection was refused or lost" : ""));
      if (!this.stopped) {
        this.retry = setTimeout(() => this.connect(), this.backoff);
        this.backoff = Math.min(this.backoff * 2, 30_000);
      }
    };
  }

  disconnect() {
    this.stopped = true;
    clearTimeout(this.retry);
    this.ws?.close(1000, "disconnected");
  }

  get open(): boolean {
    return this.ws?.readyState === WebSocket.OPEN && this.client !== null;
  }

  /** Runs `f` on the session, then sends what it queued and hands out what came of it. */
  with<T>(f: (c: Client) => T): T | undefined {
    if (!this.client) return undefined;
    const r = f(this.client);
    this.pump();
    return r;
  }

  /** A request (serde JSON of thurm_proto::Request) and its answer. */
  request(request: unknown): Promise<unknown> {
    return new Promise((resolve, reject) => {
      const id = this.with((c) => c.request(JSON.stringify(request)));
      if (id === undefined) return reject(new Error("not connected"));
      this.pending.set(id, (ok, error) => (error === undefined ? resolve(ok) : reject(new Error(error))));
    });
  }

  /** Pings and timed-out synchronized updates. */
  tick() {
    this.with((c) => c.tick());
  }

  pump() {
    const c = this.client;
    if (!c) return;
    const events = JSON.parse(c.take_events()) as Incoming[];
    const out = c.take_outgoing();
    if (out.length > 0 && this.ws?.readyState === WebSocket.OPEN) this.ws.send(out as Uint8Array<ArrayBuffer>);
    for (const ev of events) {
      if (ev.kind === "response") {
        const done = this.pending.get(ev.id);
        this.pending.delete(ev.id);
        done?.(ev.ok, ev.error);
      } else {
        this.onIncoming(ev);
      }
    }
    // A synchronized update ends on its own at its deadline.
    clearTimeout(this.timer);
    const ms = c.next_deadline_ms();
    if (ms < 1000) this.timer = setTimeout(() => this.tick(), Math.max(ms, 1));
  }
}
