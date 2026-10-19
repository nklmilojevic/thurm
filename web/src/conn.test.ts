// One socket and one session at a time, however connect() is called (reconnect timer, a tab
// coming back, the Connect button).
import { expect, test } from "bun:test";
import type { Client } from "../pkg/thurm_web.js";
import { Conn, type ConnState } from "./conn";

class FakeSocket {
  static all: FakeSocket[] = [];
  readyState: number = WebSocket.CONNECTING;
  binaryType = "blob";
  closed = false;
  sent: Uint8Array[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: ArrayBuffer }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;

  constructor() {
    FakeSocket.all.push(this);
  }
  open() {
    this.readyState = WebSocket.OPEN;
    this.onopen?.();
  }
  close() {
    if (this.closed) return;
    this.closed = true;
    this.readyState = WebSocket.CLOSED;
    this.onclose?.({ code: 1000, reason: "" });
  }
  send(b: Uint8Array) {
    this.sent.push(b);
  }
}

let clients = 0;
const fakeClient = () => {
  clients++;
  return {
    free() {},
    take_events: () => "[]",
    take_outgoing: () => new Uint8Array([1]),
    next_deadline_ms: () => 15_000,
  } as unknown as Client;
};

function setup() {
  FakeSocket.all = [];
  clients = 0;
  const states: ConnState[] = [];
  const conn = new Conn(
    () => {},
    (s) => states.push(s),
    { url: () => "ws://test/v1/connect", socket: () => new FakeSocket() as unknown as WebSocket, client: fakeClient },
  );
  return { conn, states };
}

test("a connect while the socket is opening opens no second one", () => {
  const { conn } = setup();
  conn.connect();
  conn.connect();
  expect(FakeSocket.all.length).toBe(1);
  FakeSocket.all[0].open();
  expect(clients).toBe(1);
  expect(conn.open).toBe(true);
});

test("a connect while open keeps the session", () => {
  const { conn } = setup();
  conn.connect();
  FakeSocket.all[0].open();
  const session = conn.client;
  conn.connect();
  expect(FakeSocket.all.length).toBe(1);
  expect(conn.client).toBe(session);
});

test("a socket given up on that opens late is closed, not made the session", () => {
  const { conn } = setup();
  conn.connect();
  const first = FakeSocket.all[0];
  // Closing (the user pressed Disconnect, then Connect at once): a new socket replaces it.
  first.readyState = WebSocket.CLOSING;
  conn.connect();
  const second = FakeSocket.all[1];
  second.open();
  const session = conn.client;
  // The first one's open arrives after all.
  first.onopen?.();
  expect(first.closed).toBe(true);
  expect(conn.client).toBe(session);
  expect(clients).toBe(1);
  expect(second.sent.length).toBe(1);
});

test("the session ends with its socket, and the next socket starts a new one", () => {
  const { conn, states } = setup();
  conn.connect();
  FakeSocket.all[0].open();
  conn.disconnect();
  expect(conn.client).toBeNull();
  expect(conn.open).toBe(false);
  expect(states.at(-1)).toBe("closed");
  conn.connect();
  expect(FakeSocket.all.length).toBe(2);
  FakeSocket.all[1].open();
  expect(clients).toBe(2);
});
