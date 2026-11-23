import { test, expect, CHOSEN } from "./fixtures";

// O5: race a request across two providers, compare, and merge only the one picked.

test("a race runs the request per provider, holds both, and merges only the one picked", async ({ openFixture, page, daemon }) => {
  test.setTimeout(90_000);
  const fixture = daemon.fixture!;
  await fetch(`${fixture.url}/api/settings`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${fixture.token}` },
    body: JSON.stringify({ roles: CHOSEN }),
  });
  await openFixture("/runs/new");
  await page.getByTestId("race-button").click();
  const pop = page.getByTestId("race-popover");
  await pop.getByTestId("race-grok").click();
  await pop.getByTestId("race-codex").click();
  await expect(page.getByTestId("race-button")).toContainText("Race");
  await page.keyboard.press("Escape");
  await page.getByRole("checkbox", { name: "Build without asking me to approve the plan" }).check();
  await page.locator(".composer-input").click();
  await page.keyboard.type("A small bookshop");
  await page.keyboard.press("Enter");
  await page.waitForURL(/\/races\/race-/);

  const cols = page.getByTestId("race-col");
  await expect(cols).toHaveCount(2);
  await expect(cols.nth(0).getByTestId("race-state")).toHaveText(/Passed|Failed/, { timeout: 60_000 });
  await expect(cols.nth(1).getByTestId("race-state")).toHaveText(/Passed|Failed/, { timeout: 60_000 });
  const label = await cols.filter({ has: page.getByTestId("race-state").filter({ hasText: "Passed" }) }).first().getAttribute("aria-label");
  const picked = page.getByRole("region", { name: label! });
  page.once("dialog", (d) => void d.accept());
  await picked.getByTestId("race-pick").click();
  await expect(picked.getByTestId("race-state")).toHaveText("Merged");
  await expect(page.getByTestId("race-state").filter({ hasText: "Archived" })).toHaveCount(1);
  await expect(page.getByText(/Decided: .* was merged/)).toBeVisible();
});
