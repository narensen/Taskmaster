import { test, expect, finishedRun, lowContrast } from "./fixtures";

// T3 Code-style Run View: top live headline, glimpses of reasoning (never raw response walls
// by default), Master Overall Plan with worker delegation + Inspect jump, click-to-inspect
// accessed files & activity for Master and every Worker, and 4.5:1 contrast in dark & light.

test("run view shows live headline, reasoning glimpses, Master overall plan, and click-to-inspect files", async ({ openFixture, page, daemon }) => {
  test.setTimeout(90_000);
  const headers = { "content-type": "application/json", authorization: `Bearer ${daemon.fixture!.token}` };
  const before = ((await (await fetch(`${daemon.fixture!.url}/api/settings`, { headers })).json()) as { project: string }).project;
  const made = await fetch(`${daemon.fixture!.url}/api/projects`, {
    method: "POST",
    headers,
    body: JSON.stringify({ name: `runview${Date.now().toString().slice(-6)}`, switch: true }),
  });
  expect(made.ok).toBe(true);
  try {
    await finishedRun(daemon.fixture!, "runview-t3", "Plan this small product quickly. A bookshop with home, books, and cart.");
    await openFixture("/runs/runview-t3");

    // Sticky top live headline
    await expect(page.getByTestId("run-headline")).toBeVisible();
    await expect(page.getByTestId("run-headline")).toContainText(/Completed|Merged into/);

    // Master shows its headline bar, reasoning glimpse, and Overall Plan — never raw JSON dump by default
    const master = page.locator("#agent-master");
    await expect(master).toBeVisible();
    await expect(master.getByTestId("agent-headline")).toHaveAttribute("aria-expanded", "false");
    await expect(master.getByTestId("agent-glimpse")).toContainText("The request names three pages");
    await expect(master.getByTestId("agent-activity")).toHaveCount(0);
    await expect(master).not.toContainText("which is only here to show a path word");

    // Overall Plan under Master shows all delegated workers (including split units)
    const plan = master.getByTestId("locked-plan");
    await expect(plan).toBeVisible();
    const planWorkers = plan.getByTestId("plan-workers");
    await expect(planWorkers).toContainText("home");
    await expect(planWorkers).toContainText("books / list");
    await expect(planWorkers).toContainText("books / styles");
    await expect(planWorkers).toContainText("cart");

    // Clicking Master's headline reveals accessed files and tool activity, keeping raw output collapsed
    await master.getByTestId("agent-headline").click();
    await expect(master.getByTestId("agent-headline")).toHaveAttribute("aria-expanded", "true");
    const masterActivity = master.getByTestId("agent-activity");
    await expect(masterActivity).toBeVisible();
    await expect(masterActivity.getByTestId("agent-usage")).toBeVisible();
    await expect(masterActivity.getByTestId("agent-usage")).toContainText(/out/);
    await expect(masterActivity.getByTestId("agent-usage")).toContainText(/in/);
    await expect(masterActivity.getByTestId("agent-files")).toContainText("README.md");
    await expect(masterActivity.locator("details.agent-raw")).not.toHaveAttribute("open", "");

    // Clicking Inspect on a worker in the Overall Plan opens that worker's activity and accessed files
    await planWorkers.locator(".plan-worker", { hasText: "home" }).getByRole("button", { name: "Inspect" }).click();
    const worker = page.locator("[data-agent='page:home'], [data-agent='unit:home']").first();
    await expect(worker.getByTestId("agent-headline")).toHaveAttribute("aria-expanded", "true");
    await expect(worker.getByTestId("agent-activity").getByTestId("agent-usage")).toContainText(/out/);
    await expect(worker.getByTestId("agent-activity").getByTestId("agent-files")).toContainText("home/page.tsx");
    await expect(worker.getByTestId("agent-activity").getByTestId("agent-files")).toContainText("types.ts");

    // Clicking the worker's headline again collapses it cleanly back to glimpse + file strip
    await worker.getByTestId("agent-headline").click();
    await expect(worker.getByTestId("agent-headline")).toHaveAttribute("aria-expanded", "false");
    await expect(worker.getByTestId("agent-activity")).toHaveCount(0);

    // Per-worker and total diff stats render and clicking opens the scoped Diff tab
    await expect(page.getByTestId("run-diff-stat")).toBeVisible();
    const workerDiff = worker.getByTestId("worker-diff-stat");
    await expect(workerDiff).toBeVisible();
    await expect(workerDiff).toContainText(/\+\d+/);
    await workerDiff.click();
    const rightPanel = page.getByTestId("right-panel");
    await expect(rightPanel).toBeVisible();
    await expect(rightPanel.getByRole("tab", { name: "Diff" })).toHaveAttribute("aria-selected", "true");

    // Contrast holds in both dark and light themes on the redesigned Run View
    expect(await lowContrast(page)).toEqual([]);
    await page.evaluate(() => {
      const p = JSON.parse(localStorage.getItem("taskmaster.prefs") ?? "{}");
      localStorage.setItem("taskmaster.prefs", JSON.stringify({ ...p, theme: "light" }));
    });
    await page.reload();
    await expect(page.locator(".shell")).toHaveClass(/theme-light/);
    await expect(page.getByTestId("run-headline")).toBeVisible();
    await page.locator("#agent-master").getByTestId("agent-headline").click();
    await expect(page.locator("#agent-master").getByTestId("agent-usage")).toBeVisible();
    expect(await lowContrast(page)).toEqual([]);
  } finally {
    await fetch(`${daemon.fixture!.url}/api/projects/open`, {
      method: "POST",
      headers,
      body: JSON.stringify({ path: before }),
    });
  }
});

test("a merged run is undone from its header after one confirm, and says so after a reload", async ({ openFixture, page, daemon }) => {
  test.setTimeout(90_000);
  const headers = { "content-type": "application/json", authorization: `Bearer ${daemon.fixture!.token}` };
  const before = ((await (await fetch(`${daemon.fixture!.url}/api/settings`, { headers })).json()) as { project: string }).project;
  const made = await fetch(`${daemon.fixture!.url}/api/projects`, {
    method: "POST",
    headers,
    body: JSON.stringify({ name: `undo${Date.now().toString().slice(-6)}`, switch: true }),
  });
  expect(made.ok).toBe(true);
  try {
    await finishedRun(daemon.fixture!, "undo-me", "Plan this small product quickly. A bookshop with home, books, and cart.");
    await openFixture("/runs/undo-me");
    const merged = page.getByTestId("merged-into");
    await expect(merged).toBeVisible();
    await merged.getByTestId("undo-merge").click();
    await expect(merged).toContainText("Put back every file it changed");
    await merged.getByTestId("undo-merge-confirm").click();
    await expect(merged.getByTestId("run-undone")).toContainText(/undone in [0-9a-f]{7,}/);
    await page.reload();
    await expect(page.getByTestId("merged-into").getByTestId("run-undone")).toBeVisible();
    await expect(page.getByTestId("undo-merge")).toHaveCount(0);
    const again = await fetch(`${daemon.fixture!.url}/api/runs/undo-me/undo`, { method: "POST", headers, body: "{}" });
    expect(again.status).toBe(409);
    expect(((await again.json()) as { error: string }).error).toContain("already undone");
  } finally {
    await fetch(`${daemon.fixture!.url}/api/projects/open`, { method: "POST", headers, body: JSON.stringify({ path: before }) });
  }
});
