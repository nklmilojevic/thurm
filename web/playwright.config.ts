import { defineConfig, devices } from "@playwright/test";

// End-to-end: the built page (`just web`) against a real thurmd on an isolated socket, behind
// the thurm-webtest gateway (e2e/harness.ts). One daemon per test file, so one worker.
export default defineConfig({
  testDir: "e2e",
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: [["list"]],
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
  ],
});
