// thurm-pi-extension: 1
// Installed by `thurm hooks install --agent pi`. Remove with `thurm hooks uninstall --agent pi`.

import { spawn } from "node:child_process";
import path from "node:path";

const MAX_STDOUT = 64 * 1024;

export default function (pi) {
  if (!process.env.THURM_PANE_ID) return;

  const bin = process.env.THURM_BIN_DIR
    ? path.join(process.env.THURM_BIN_DIR, "thurm")
    : "thurm";

  let sessionContext = "";
  let working = false;
  let chain = Promise.resolve();

  function runHook(event, payload, timeoutMs = 5000) {
    return new Promise((resolve) => {
      let settled = false;
      const done = (out) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve(out);
      };

      let child;
      try {
        child = spawn(bin, ["agent-hook", "pi", event], {
          stdio: ["pipe", "pipe", "ignore"],
        });
      } catch {
        done("");
        return;
      }

      const timer = setTimeout(() => {
        try {
          child.kill();
        } catch {}
        done("");
      }, timeoutMs);
      timer.unref?.();

      const chunks = [];
      let bytes = 0;
      child.stdout?.on("data", (chunk) => {
        if (bytes >= MAX_STDOUT) return;
        const slice = chunk.subarray(0, MAX_STDOUT - bytes);
        chunks.push(slice);
        bytes += slice.length;
      });
      child.on("error", () => done(""));
      child.on("close", () => done(Buffer.concat(chunks).toString("utf8")));

      try {
        child.stdin?.on("error", () => {});
        child.stdin?.end(JSON.stringify(payload));
      } catch {}
    });
  }

  function enqueue(event, payload, timeoutMs) {
    const next = chain.then(() => runHook(event, payload, timeoutMs));
    chain = next.then(
      () => {},
      () => {},
    );
    return next;
  }

  function sessionPayload(ctx, extra = {}) {
    const file = ctx?.sessionManager?.getSessionFile?.() || undefined;
    return file
      ? { session_id: file, transcript_path: file, ...extra }
      : { ...extra };
  }

  pi.on("session_start", async (_event, ctx) => {
    working = false;
    sessionContext = (
      await enqueue("session-start", sessionPayload(ctx))
    ).trim();
  });

  pi.on("session_info_changed", async (_event, ctx) => {
    void enqueue("session-info", sessionPayload(ctx));
  });

  pi.on("before_agent_start", async (event, ctx) => {
    working = true;
    const prompt =
      typeof event?.prompt === "string" && event.prompt.trim()
        ? event.prompt
        : undefined;
    void enqueue(
      "prompt-submit",
      sessionPayload(ctx, prompt ? { prompt } : {}),
    );
    if (!sessionContext) return;
    if (event?.systemPromptOptions?.sections) {
      event.systemPromptOptions.sections.thurm = sessionContext;
      return;
    }
    if (typeof event?.systemPrompt === "string") {
      return { systemPrompt: `${event.systemPrompt}\n\n${sessionContext}` };
    }
  });

  pi.on("agent_settled", async (_event, ctx) => {
    working = false;
    void enqueue("stop", sessionPayload(ctx));
  });

  pi.on("ui_prompt_start", async (event, ctx) => {
    const message =
      typeof event?.title === "string" && event.title.trim()
        ? event.title.trim()
        : undefined;
    void enqueue(
      "notification",
      sessionPayload(ctx, message ? { message } : {}),
    );
  });

  pi.on("ui_prompt_end", async (_event, ctx) => {
    if (!working) return;
    void enqueue("tool-complete", sessionPayload(ctx));
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    working = false;
    await enqueue("session-end", sessionPayload(ctx), 1000);
  });
}
