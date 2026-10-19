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

/** What a connection is made of; tests swap in fakes. */
export type ConnDeps = {
  url: () => string;
  socket: (url: string) => WebSocket;
  client: () => Client;
  now: () => number;
};

const browserDeps: ConnDeps = {
  url: () => `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/v1/connect`,
  socket: (url) => new WebSocket(url),
  // The terminal's default theme on the page's side of light and dark.
  client: () => new Client(matchMedia("(prefers-color-scheme: dark)").matches),
  now: () => Date.now(),
};

export class Conn {
  client: Client | null = null;
  private ws: WebSocket | null = null;
  private pending = new Map<number, (ok: unknown, error?: string) => void>();
  private backoff = 1000;
  private stopped = false;
  private retry: ReturnType<typeof setTimeout> | undefined;
  /** When the next attempt is due after a close (0: none): the worker clock's ticks retry too. */
  private retryAt = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private onIncoming: (msg: Incoming) => void,
    private onState: (state: ConnState, detail?: string) => void,
    private deps: ConnDeps = browserDeps,
  ) {}

  static async load() {
    await init({ module_or_path: __WASM_URL__ });
  }

  /** Opens the connection, unless one is open or opening: one socket, one session. */
  connect() {
    this.stopped = false;
    clearTimeout(this.retry);
    this.retryAt = 0;
    const state = this.ws?.readyState;
    if (state === WebSocket.CONNECTING || state === WebSocket.OPEN) return;
    this.onState("connecting");
    const ws = this.deps.socket(this.deps.url());
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = () => {
      // A socket this connection gave up on (it was closing when connect ran again).
      if (this.ws !== ws) {
        ws.close();
        return;
      }
      this.backoff = 1000;
      this.client?.free();
      this.client = this.deps.client();
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
      // Its session ends with it; the next socket starts a new one.
      this.client?.free();
      this.client = null;
      for (const done of this.pending.values()) done(undefined, "disconnected");
      this.pending.clear();
      this.onState("closed", ev.reason || (ev.code === 1006 ? "the connection was refused or lost" : ""));
      if (!this.stopped) {
        // A hidden tab's timers may be held back for a minute or more; `tick`, which the
        // worker clock drives, retries on time anyway.
        this.retryAt = this.deps.now() + this.backoff;
        this.retry = setTimeout(() => this.connect(), this.backoff);
        this.backoff = Math.min(this.backoff * 2, 30_000);
      }
    };
  }

  disconnect() {
    this.stopped = true;
    clearTimeout(this.retry);
    this.retryAt = 0;
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
    if (!this.ws && !this.stopped && this.retryAt && this.deps.now() >= this.retryAt) this.connect();
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
