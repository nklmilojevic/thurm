import { type Page, expect, test } from "@playwright/test";
import { type Harness, start } from "./harness";

let h: Harness;
let pane: number;

test.beforeAll(async () => {
  h = await start();
  pane = h.newPane();
});

test.afterAll(() => h?.stop());

// What the page says when it breaks, in the test's output.
test.beforeEach(({ page }) => {
  page.on("console", (m) => {
    if (m.type() === "error" || m.type() === "warning") console.log(`[page ${m.type()}] ${m.text()}`);
  });
  page.on("pageerror", (e) => console.log(`[page error] ${e.message}`));
});

const text = (page: Page) => page.evaluate(() => (window as any).__thurmTest.text() as string);

async function open(page: Page) {
  await page.goto(h.url);
  await expect.poll(() => page.evaluate(() => (window as any).__thurmTest.panes().length)).toBeGreaterThan(0);
  await expect.poll(() => page.evaluate(() => (window as any).__thurmTest.focusedPane())).not.toBeNull();
  await expect.poll(() => text(page)).toContain("$");
}

test("attaches to a pane and types into it", async ({ page }) => {
  await open(page);
  await page.keyboard.type("echo web-$((40+2))");
  await page.keyboard.press("Enter");
  await expect.poll(() => text(page)).toContain("web-42");
});

test("shows what another client types into the same pane", async ({ page }) => {
  await open(page);
  h.thurm("send", "--pane", String(pane), "echo from-cli-$((1+1))");
  await expect.poll(() => text(page)).toContain("from-cli-2");
});

test("pastes multi-line text", async ({ page }) => {
  await open(page);
  await page.evaluate(() => {
    const data = new DataTransfer();
    data.setData("text/plain", "echo one-$((0+1))\necho two-$((1+1))\n");
    document.getElementById("input")!.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true }));
  });
  await expect.poll(() => text(page)).toContain("one-1");
  await expect.poll(() => text(page)).toContain("two-2");
});

test("scrolls back through history with the wheel", async ({ page }) => {
  await open(page);
  await page.keyboard.type("seq 1 300");
  await page.keyboard.press("Enter");
  await expect.poll(() => text(page)).toContain("300");
  const canvas = page.locator(".pane canvas").first();
  await canvas.hover();
  for (let i = 0; i < 40; i++) await page.mouse.wheel(0, -400);
  await expect.poll(async () => (await text(page)).split("\n").includes("1")).toBe(true);
});

test("ends a selection on release", async ({ page }) => {
  await open(page);
  await page.keyboard.type("echo select-$((6*7))");
  await page.keyboard.press("Enter");
  await expect.poll(() => text(page)).toContain("select-42");
  const box = (await page.locator(".pane canvas").first().boundingBox())!;
  const metrics = await page.evaluate(() => (window as any).__thurmTest.cell());
  const row = (await text(page)).split("\n").indexOf("select-42");
  const y = box.y + (row + 0.5) * metrics.h;
  await page.mouse.move(box.x + 2, y);
  await page.mouse.down();
  await page.mouse.move(box.x + metrics.w * 5.6, y);
  await page.mouse.up();
  const selection = () => page.evaluate(() => (window as any).__thurmTest.selection() as string);
  // Where a drag ends inside a cell differs a little between engines; what it selected is
  // "select" or one cell either side.
  await expect.poll(selection).toMatch(/^sele/);
  const selected = await selection();
  // Moving after the release leaves the selection as it was.
  await page.mouse.move(box.x + metrics.w * 3, y + metrics.h * 2);
  await page.mouse.move(box.x + metrics.w * 1, y + metrics.h * 3);
  expect(await selection()).toBe(selected);
});

test("types through an input method", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "IME composition through CDP");
  await open(page);
  const cdp = await page.context().newCDPSession(page);
  await page.keyboard.type("echo ");
  await cdp.send("Input.imeSetComposition", { text: "にほ", selectionStart: 2, selectionEnd: 2 });
  await cdp.send("Input.insertText", { text: "日本" });
  await page.keyboard.press("Enter");
  await expect.poll(() => text(page)).toContain("日本");
});

test("lists agent and program notices", async ({ page }) => {
  await open(page);
  h.thurm("send", "--pane", String(pane), "printf '\\033]777;notify;Build done;All green\\a'");
  await expect.poll(() => page.evaluate(() => (window as any).__thurmTest.notices())).toContainEqual(
    expect.objectContaining({ pane, title: "Build done", body: "All green" }),
  );
  await expect(page.locator(".notice .title", { hasText: "Build done" })).toBeVisible();
});

test("reconnects when the daemon comes back", async ({ page }) => {
  await open(page);
  await h.restartDaemon();
  pane = h.newPane();
  await expect.poll(() => page.evaluate(() => (window as any).__thurmTest.connected()), { timeout: 30_000 }).toBe(true);
  await expect.poll(() => page.evaluate(() => (window as any).__thurmTest.panes()), { timeout: 30_000 }).toContain(pane);
  await expect.poll(() => text(page)).toContain("$");
});
