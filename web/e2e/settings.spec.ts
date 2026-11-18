import { test, expect } from "./fixtures";

test("the mid-tier checker is off by default and its switch is kept by the daemon", async ({ open, page, daemon }) => {
  await open("/settings");
  const toggle = page.getByTestId("checker-setting").getByRole("switch", { name: "Check finished pages with the mid model" });
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  const saved = (await (await fetch(`${daemon.url}/api/settings`, { headers: { authorization: `Bearer ${daemon.token}` } })).json()) as { checker: { enabled: boolean } };
  expect(saved.checker.enabled).toBe(true);
  await page.reload();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await toggle.focus();
  await page.keyboard.press("Space");
  await expect(toggle).toHaveAttribute("aria-checked", "false");
});

test("Code Mode is off by default and can be switched on", async ({ open, page }) => {
  await open("/settings");
  const toggle = page.getByTestId("code-mode-setting").getByRole("switch", { name: "Use Code Mode for workers" });
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await page.reload();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-checked", "false");
});
