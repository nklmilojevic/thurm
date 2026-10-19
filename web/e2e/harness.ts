// A thurmd of its own (socket, config and state under a short /tmp dir, never the user's)
// behind the thurm-webtest gateway, which admits every caller as the owner and serves the
// built page.
import { type ChildProcess, execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const repo = resolve(import.meta.dirname, "../..");
const dist = join(repo, "tailnet/internal/web/dist");

export type Harness = {
  url: string;
  env: NodeJS.ProcessEnv;
  thurm: (...args: string[]) => string;
  newPane: () => number;
  restartDaemon: () => Promise<void>;
  stop: () => void;
};

async function until(what: string, ok: () => boolean, ms = 15_000) {
  const end = Date.now() + ms;
  while (!ok()) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

export async function start(): Promise<Harness> {
  if (!existsSync(join(dist, "index.html"))) throw new Error("no web bundle: run `just web` first");
  const dir = mkdtempSync("/tmp/thw-");
  for (const d of ["c", "s", "r"]) mkdirSync(join(dir, d));
  const socket = join(dir, "d.sock");
  writeFileSync(join(dir, "c/config.toml"), "");
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    THURM_SOCKET: socket,
    THURM_CONFIG_DIR: join(dir, "c"),
    THURM_STATE_DIR: join(dir, "s"),
    THURM_REMOTE_DIR: join(dir, "r"),
    THURM_DAEMON: join(repo, "target/debug/thurmd"),
    // The daemon drops a quiet mobile client after 45 s; shorter here, so the keepalive test
    // need not wait that long. Still above the page's 15 s pings plus its 5 s clock.
    THURM_MOBILE_SILENCE_MS: "25000",
  };
  const procs: ChildProcess[] = [];
  let daemon: ChildProcess;
  const startDaemon = async () => {
    daemon = spawn(join(repo, "target/debug/thurmd"), ["--foreground", "--no-restore", "--socket", socket], {
      env,
      stdio: "ignore",
    });
    procs.push(daemon);
    await until("the daemon socket", () => existsSync(socket));
  };
  await startDaemon();

  const gateway = join(dir, "webtest");
  execFileSync("go", ["build", "-o", gateway, "./cmd/thurm-webtest"], {
    cwd: join(repo, "tailnet"),
    env: { ...process.env, CGO_ENABLED: "0" },
  });
  const web = spawn(gateway, ["--socket", socket, "--dist", dist], { env, stdio: ["ignore", "pipe", "inherit"] });
  procs.push(web);
  const url = await new Promise<string>((ok, fail) => {
    let buf = "";
    web.stdout!.on("data", (d) => {
      buf += d;
      const m = buf.match(/listening on (http:\/\/\S+)/);
      if (m) ok(m[1]);
    });
    web.on("exit", (code) => fail(new Error(`thurm-webtest exited (${code})`)));
  });

  const thurm = (...args: string[]) =>
    execFileSync(join(repo, "target/debug/thurm"), args, { env, encoding: "utf8" }).trim();
  return {
    url,
    env,
    thurm,
    // Not the user's login shell and its prompt: a plain sh with a fixed one.
    newPane: () =>
      Number(JSON.parse(thurm("new-tab", "--json", "--cwd", dir, "--", "/usr/bin/env", "PS1=$ ", "/bin/sh")).pane),
    restartDaemon: async () => {
      daemon.kill("SIGKILL");
      await until("the daemon to exit", () => daemon.exitCode !== null || daemon.signalCode !== null);
      rmSync(socket, { force: true });
      await startDaemon();
    },
    stop: () => {
      for (const p of procs) p.kill("SIGKILL");
      rmSync(dir, { recursive: true, force: true });
    },
  };
}
