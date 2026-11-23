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

test("Ctrl+K finds words said in a run and opens the run at them, marked", async ({ openFixture, page, daemon }) => {
  test.setTimeout(60_000);
  const fixture = daemon.fixture!;
  await finishedRun(fixture, "said-search", "A bookshop with a cart");
  // A follow-up only this run has, so the search is about this run whatever else ran.
  const headers = { "content-type": "application/json", authorization: `Bearer ${fixture.token}` };
  const noted = await fetch(`${fixture.url}/api/runs/said-search/notes`, {
    method: "POST",
    headers,
    body: JSON.stringify({ to: "master", text: "Paint the zebrafish banner teal", mode: "agent" }),
  });
  expect(noted.status).toBe(200);
  await expect
    .poll(async () => (await (await fetch(`${fixture.url}/api/runs/said-search`, { headers })).json()).active, { timeout: 30_000 })
    .toBe(false);
  await openFixture("/");
  await page.keyboard.press("Control+k");
  const palette = page.getByRole("dialog", { name: "Command palette" });
  await expect(palette).toBeVisible();
  await page.keyboard.type("zebrafis");
  const hit = palette.getByTestId("palette-said").filter({ hasText: "zebrafish banner" }).first();
  await expect(hit).toBeVisible();
  await expect(hit).toContainText("A bookshop with a cart");
  await expect(hit).toContainText("turn 2");
  await hit.click();
  await page.waitForURL(/\/runs\/said-search$/);
  await expect.poll(() => page.evaluate(() => (CSS as unknown as { highlights?: Map<string, Set<unknown>> }).highlights?.get("run-hit")?.size ?? 0)).toBeGreaterThan(0);
});

test("Settle lives on the daemon: a cleared browser still shows the run settled", async ({ openFixture, page, daemon }) => {
  test.setTimeout(60_000);
  await finishedRun(daemon.fixture!, "flag-run", "A page to settle");
  await openFixture("/");
  const row = page.locator(".runrow", { hasText: "flag-run" });
  await expect(row).toBeVisible();
  await row.hover();
  await row.getByRole("button", { name: "Settle" }).click();
  await expect(row).toHaveClass(/settled/);
  const headers = { authorization: `Bearer ${daemon.fixture!.token}` };
  const run = await (await fetch(`${daemon.fixture!.url}/api/runs/flag-run`, { headers })).json();
  expect(run.flags).toMatchObject({ settled: true, settled_by: "you" });
  // Another device has none of this browser's prefs.
  await page.evaluate(() => localStorage.removeItem("taskmaster.prefs"));
  await page.reload();
  await expect(page.locator(".runrow", { hasText: "flag-run" })).toHaveClass(/settled/);
  await page.locator(".runrow", { hasText: "flag-run" }).hover();
  await page.locator(".runrow", { hasText: "flag-run" }).getByRole("button", { name: "Unsettle" }).click();
  await expect(page.locator(".runrow", { hasText: "flag-run" })).not.toHaveClass(/settled/);
});

test("Labs: a finished run's composer offers a suggested next message, and Tab takes it", async ({ openFixture, page, daemon }) => {
  // The scripted fixture never answers a suggestion call, so the suggestion is served as the daemon would send it.
  await finishedRun(daemon.fixture!, "suggest-run", "A small shop");
  await page.route("**/api/runs/suggest-run", async (route) => {
    const response = await route.fetch();
    const body = await response.json();
    await route.fulfill({ response, json: { ...body, suggestion: { text: "Add a test for the empty cart", at: "2026-11-23T00:00:00Z" } } });
  });
  await openFixture("/runs/suggest-run");
  const box = page.locator(".composer-input");
  await expect(box).toHaveAttribute("data-placeholder", /Add a test for the empty cart/);
  await box.click();
  await page.keyboard.press("Tab");
  await expect(box).toHaveText("Add a test for the empty cart");
});

test("a finished run can give a seat to another provider for the next turn", async ({ openFixture, page, daemon }) => {
  test.setTimeout(60_000);
  await finishedRun(daemon.fixture!, "switch-run", "A page to hand over");
  let sent: unknown = null;
  await page.route("**/api/runs/switch-run/roles", async (route) => {
    sent = route.request().postDataJSON();
    await route.fulfill({ json: { roles: {}, models: {}, handoff: { to: { worker: "grok" } } } });
  });
  await openFixture("/runs/switch-run");
  await page.getByTestId("switch-providers").click();
  const pop = page.getByTestId("switch-popover");
  await expect(pop.getByRole("button", { name: /^Worker provider for the next turn: Grok/ })).toBeVisible();
  await pop.getByRole("button", { name: /^Worker provider for the next turn/ }).click();
  const prov = page.locator(".prov-pop").last();
  await prov.getByRole("listbox", { name: "Providers" }).getByRole("option", { name: /Grok/ }).click({ timeout: 5000 });
  await prov.getByRole("option", { name: /Provider default/ }).click({ timeout: 5000 });
  await expect.poll(() => sent).toEqual({ roles: { worker: "grok" }, models: { worker: "" } });
  await expect(page.getByText("Worker switched")).toBeVisible();
});
