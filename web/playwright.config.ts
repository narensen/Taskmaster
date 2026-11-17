import { defineConfig } from "@playwright/test";

// End-to-end tests: the built app served by a real tm-daemon on a throwaway project.
// `npm run e2e` builds both first. Chrome is the system one (TASKMASTER_CHROME to change it).
export default defineConfig({
  testDir: "e2e",
  globalSetup: "./e2e/setup.ts",
  globalTeardown: "./e2e/teardown.ts",
  fullyParallel: false,
  workers: 1,
  timeout: 30_000,
  reporter: [["list"]],
  use: {
    launchOptions: { executablePath: process.env.TASKMASTER_CHROME ?? "/usr/bin/google-chrome" },
    viewport: { width: 1360, height: 860 },
    colorScheme: "dark",
  },
});
