import { test, expect } from "./fixtures";

test("Settings ops panels save schedules, vault MCP secrets, run GC, and Home imports issue URLs", async ({ open, page }) => {
  await open("/settings");

  // 1. Add and persist a scheduled run (Operations tab)
  await page.getByTestId("settings-tab-operations").click();
  const schedules = page.getByTestId("ops-schedules");
  await expect(schedules).toBeVisible();
  await schedules.getByPlaceholder("Name (e.g. Nightly audit)").fill("Weekly sweep");
  await schedules.getByPlaceholder("Prompt for the scheduled run").fill("Check for flaky tests");
  await schedules.getByRole("button", { name: "Add schedule" }).click();
  await expect(schedules).toContainText("Weekly sweep");

  // 2. Operations panel shows monthly spend + runs retention GC
  const ops = page.getByTestId("ops-overview");
  await expect(ops).toContainText("Monthly spend");
  await ops.getByRole("button", { name: "Run retention GC" }).click();
  await expect(ops).toContainText("GC: removed");

  // 3. Add a user MCP server with a vaulted secret env value (Tools tab)
  await page.getByTestId("settings-tab-tools").click();
  const mcp = page.getByTestId("ops-mcp");
  await mcp.getByPlaceholder("Server name (e.g. postgres)").fill("sentry");
  await mcp.getByPlaceholder("Command (e.g. npx)").fill("npx");
  await mcp.getByPlaceholder("Secret env key (optional)").fill("SENTRY_TOKEN");
  await mcp.getByPlaceholder("Secret env value (vaulted)").fill("sntrys-super-secret-value-999");
  await mcp.getByRole("button", { name: "Add MCP server" }).click();
  await expect(mcp).toContainText("sentry");
  await expect(mcp).toContainText("vaulted env: SENTRY_TOKEN");
  await expect(mcp).not.toContainText("sntrys-super-secret-value-999");

  // 4. Reload persists MCP server & schedule without leaking secret
  await page.reload();
  await expect(page.getByTestId("ops-mcp")).toContainText("vaulted env: SENTRY_TOKEN");
  await expect(page.getByTestId("ops-mcp")).not.toContainText("sntrys-super-secret-value-999");
  await page.getByTestId("settings-tab-operations").click();
  await expect(page.getByTestId("ops-schedules")).toContainText("Weekly sweep");

  // 5. New run launcher opens issue import bar and parses a GitHub issue URL
  await open("/runs/new");
  await page.getByRole("button", { name: "Import issue" }).click();
  const bar = page.getByTestId("issue-import-bar");
  await expect(bar).toBeVisible();
  await bar.getByPlaceholder(/Paste GitHub or GitLab issue URL/).fill("https://github.com/acme/demo/issues/42");
  await bar.getByRole("button", { name: "Import", exact: true }).click();
  await expect(bar).toHaveCount(0);
  await expect(page.getByRole("textbox")).toContainText("Closes #42");
});
