import { expect } from "@playwright/test";
import { test } from "./fixtures";

test("a pairing link signs a second browser in, and revoking it signs it out", async ({ open, page, browser, daemon }) => {
  await open("/connect");
  await expect(page.getByTestId("connect-reach")).toContainText("This computer");
  await page.getByTestId("pair-label").fill("Test phone");
  await page.getByTestId("pair-make").click();
  await expect(page.getByTestId("qr-code")).toBeVisible();
  const link = (await page.getByTestId("pair-link").textContent())!.trim();
  expect(link).toMatch(/\/pair#code=[A-Z2-9]{16}$/);

  // Another browser with no token opens the link: it is signed in with nothing to type.
  const other = await browser.newContext();
  const phone = await other.newPage();
  await phone.goto(link);
  await expect(phone.getByTestId("home-heading")).toBeVisible();
  expect(phone.url()).not.toContain("code=");
  expect(await phone.evaluate(() => localStorage.getItem("taskmaster.token"))).toMatch(/^[0-9a-f]{64}$/);

  // The first browser sees it by its name, and revokes it.
  const row = page.getByTestId("device-row").filter({ hasText: "Test phone" });
  await expect(row).toBeVisible({ timeout: 10_000 });
  await expect(row).toHaveAttribute("data-kind", "phone");
  await row.getByTestId("device-revoke").click();
  await row.getByTestId("device-revoke").click();
  await expect(row).toHaveCount(0);
  await phone.goto(daemon.url + "/settings");
  await expect(phone.getByTestId("pair-submit")).toBeVisible();
  await other.close();
});

test("an outside agent gets a token and a setup that works", async ({ open, page }) => {
  await open("/connect#agents");
  await page.getByTestId("agent-label").fill("Claude Code (test)");
  await page.getByTestId("agent-make").click();
  const snippet = page.getByTestId("agent-snippet");
  await expect(snippet).toContainText("claude mcp add --transport http taskmaster");
  await expect(snippet).toContainText("/mcp");
  await page.getByTestId("agent-test").click();
  await expect(page.getByTestId("agent-test-result")).toContainText(/It works: \d+ tools/);
  await expect(page.getByTestId("device-row").filter({ hasText: "Claude Code (test)" })).toHaveAttribute("data-kind", "agent");
  // The skill install line points at this daemon's /skill.md, which answers without a token.
  const skill = page.getByTestId("agent-skill");
  await expect(skill).toContainText("~/.claude/skills/taskmaster/SKILL.md");
  const href = await skill.getByRole("link", { name: "same file" }).getAttribute("href");
  const served = await fetch(href!);
  expect(served.status).toBe(200);
  expect(await served.text()).toContain("name: taskmaster");
});

test("a prompt link fills New Run without sending, and drops hidden characters", async ({ open, page }) => {
  await open(`/runs/new?prompt=${encodeURIComponent("Build a clock​ page")}&mode=agent`);
  await expect(page.locator(".composer-input")).toHaveText("Build a clock page");
  await expect(page.getByTestId("mode-agent")).toHaveAttribute("aria-checked", "true");
  await expect(page.getByText("Hidden characters removed")).toBeVisible();
  expect(new URL(page.url()).search).toBe("");
  // Restore the default mode for the other tests in this browser.
  await page.getByTestId("mode-auto").click();
});

test("losing the daemon shows a line that clears when it answers again", async ({ open, page }) => {
  await open("/connect");
  await expect(page.getByTestId("connect-reach")).toBeVisible();
  await page.route("**/api/**", (route) => route.abort());
  await page.evaluate(() => fetch("/api/health").catch(() => undefined));
  await page.getByRole("link", { name: "Settings" }).first().click();
  await expect(page.getByTestId("connection-banner")).toBeVisible();
  await page.unroute("**/api/**");
  await page.getByTestId("connection-banner").getByRole("button", { name: "Retry now" }).click();
  await expect(page.getByTestId("connection-banner")).toHaveCount(0);
});
