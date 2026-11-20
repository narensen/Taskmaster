import { test, expect, finishedRun } from "./fixtures";

// A run built a phase at a time shows its sketch under the master: each phase with where it
// stands, and the phase being built open with its units and its test command.

test("the run view lists the sketch's phases with their states", async ({ openFixture, page, daemon }) => {
  test.setTimeout(90_000);
  await finishedRun(daemon.fixture!, "phases-view", "A bookshop with a home page, a list of books and a cart.");
  await page.route("**/api/runs/phases-view", async (route) => {
    const response = await route.fetch();
    const run = await response.json();
    run.phase = "building";
    run.active = false;
    run.phase_at = 1;
    run.next_phase = null;
    run.plan.phases = [
      { id: "core", goal: "The shared shell and the home page." },
      { id: "shop", goal: "The list of books and the cart.", test: "npm test" },
      { id: "polish", goal: "Empty states and keyboard use." },
    ];
    run.plan.pages = run.plan.pages.map((p: { id: string }) => ({ ...p, phase: p.id === "home" ? 0 : 1 }));
    await route.fulfill({ response, json: run });
  });
  await openFixture("/runs/phases-view");
  const phases = page.getByTestId("phases");
  await expect(phases).toBeVisible();
  await expect(phases.locator("li")).toHaveCount(3);
  await expect(phases.locator("li[data-state=done]")).toContainText("core");
  const building = phases.locator("li[data-state=building]");
  await expect(building).toContainText("shop");
  await expect(building).toContainText("books · cart");
  await expect(building).toContainText("$ npm test");
  await expect(phases.locator("li[data-state=waiting]")).toContainText("polish");
});
