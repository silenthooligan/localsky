import { defineConfig, devices } from "@playwright/test";

// Where the tests point. Default is the local demo container
// (docker: LOCALSKY_DEMO=1 on :8091); override for the post-deploy canary:
//   BASE_URL=https://demo.localsky.io npx playwright test
const baseURL = process.env.BASE_URL ?? "http://localhost:8091";

export default defineConfig({
  testDir: ".",
  testMatch: /.*\.spec\.ts/,
  // A blank/broken page should fail fast, not hang for the default 30s.
  timeout: 20_000,
  expect: { timeout: 7_000 },
  // Screenshots are the load-bearing artifact here (catch "renders blank"),
  // so keep runs deterministic: no retries locally, one retry in CI for flake.
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI
    ? [["list"], ["html", { open: "never" }]]
    : [["list"]],
  use: {
    baseURL,
    // CI runs on Void Linux where Playwright's downloaded chromium cannot
    // resolve its Ubuntu-flavored deps; point at the distro browser instead.
    // The CI container runs as root with a small /dev/shm, which crashes a
    // sandboxed chromium mid-suite ("browser has been closed"), hence the
    // container-standard flags there; local runs stay fully sandboxed.
    launchOptions: {
      executablePath: process.env.PW_CHROMIUM_PATH || undefined,
      args: process.env.CI
        ? ["--no-sandbox", "--disable-dev-shm-usage"]
        : [],
    },
    // Fixed viewport so screenshot baselines are stable across machines.
    viewport: { width: 1280, height: 900 },
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
    // The app renders in the viewer's theme; pin dark so baselines are
    // deterministic across machines.
    colorScheme: "dark",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
});
