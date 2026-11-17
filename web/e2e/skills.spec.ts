import { test, expect } from "./fixtures";

// The skills store against the real daemon (its own store, seeded with tidy-css). Search and
// install are answered here so a test never reaches skills.sh or GitHub.

test("the store lists its skills and the switch turns one off and on", async ({ open, page }) => {
  await page.route("**/api/skills/updates", (route) => route.fulfill({ json: { updates: { "tidy-css": "v2" } } }));
  await open("/skills");
  const row = page.getByTestId("skill").filter({ hasText: "tidy-css" });
  await expect(row).toContainText("Keep stylesheets ordered and small.");
  await expect(row).toContainText("acme/kit");
  await expect(row).toContainText("Update available");
  const toggle = row.getByRole("switch", { name: "Offer tidy-css to the planner" });
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  await toggle.press("Space");
  await expect(toggle).toHaveAttribute("aria-checked", "true");
});

test("search shows published skills and Install adds one", async ({ open, page }) => {
  const installed: unknown[] = [];
  await page.route("**/api/skills/search?*", (route) =>
    route.fulfill({
      json: { hits: [{ id: "acme/kit/a11y-audit", name: "a11y-audit", repo: "acme/kit", skill: "a11y-audit", installs: 12400, installed: false }] },
    }),
  );
  await page.route("**/api/skills/install", (route) => {
    installed.push(route.request().postDataJSON());
    return route.fulfill({ json: { name: "a11y-audit", description: "Check pages for WCAG issues.", place: "store", path: "/x", enabled: true, source: null, installed: null, updated: null } });
  });
  await open("/skills?q=audit");
  const hit = page.getByTestId("skill-hit").filter({ hasText: "a11y-audit" });
  await expect(hit).toContainText("12k installs");
  await hit.getByRole("button", { name: "Install" }).click();
  await expect(hit).toContainText("Installed");
  expect(installed).toEqual([{ repo: "acme/kit", skill: "a11y-audit" }]);
});

test("$ in the composer opens the skill picker and inserts a skill chip", async ({ open, page }) => {
  await open("/");
  const box = page.getByRole("textbox", { name: /Tell the master/ });
  await box.click();
  await page.keyboard.type("Build a notes app with $");
  const picker = page.getByRole("listbox");
  await expect(picker.getByRole("option", { name: /tidy-css/ })).toBeVisible();
  await expect(picker.getByRole("option", { name: /Find more skills/ })).toBeVisible();
  await page.keyboard.type("tid");
  await page.keyboard.press("Enter");
  await expect(box.locator(".chip-skill")).toHaveText("$tidy-css");
  // Money is not a skill.
  await page.keyboard.type(" for $5");
  await expect(page.getByRole("listbox")).toHaveCount(0);
});
