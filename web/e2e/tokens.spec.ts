import type { Page } from "@playwright/test";
import { test, expect, finishedRun, type Daemon } from "./fixtures";

// A run on the scripted daemon, from plan to done, read back in the run view.

test("the run view splits every agent's tokens four ways", async ({ openFixture, page, daemon }) => {
  test.setTimeout(90_000);
  // A project of its own: in the fixture's shared one an earlier run has already built these
  // pages, and an unchanged page is not built again.
  const headers = { "content-type": "application/json", authorization: `Bearer ${daemon.fixture!.token}` };
  const before = ((await (await fetch(`${daemon.fixture!.url}/api/settings`, { headers })).json()) as { project: string }).project;
  const made = await fetch(`${daemon.fixture!.url}/api/projects`, { method: "POST", headers, body: JSON.stringify({ name: `tokens${Date.now().toString().slice(-6)}`, switch: true }) });
  expect(made.ok).toBe(true);
  try {
    await tokenSplit(openFixture, page, daemon);
  } finally {
    await fetch(`${daemon.fixture!.url}/api/projects/open`, { method: "POST", headers, body: JSON.stringify({ path: before }) });
  }
});

async function tokenSplit(openFixture: (path: string) => Promise<Page>, page: Page, daemon: Daemon) {
  await finishedRun(daemon.fixture!, "token-split", "A bookshop with a home page, a list of books and a cart.");
  const ledger = (await (
    await fetch(`${daemon.fixture!.url}/api/runs/token-split/ledger`, { headers: { authorization: `Bearer ${daemon.fixture!.token}` } })
  ).json()) as { totals: { input: number; output: number; cache_read: number; cache_write: number; by_agent: Record<string, unknown> } };
  expect(ledger.totals.output).toBeGreaterThan(0);
  expect(Object.keys(ledger.totals.by_agent)).toContain("master");

  await openFixture("/runs/token-split");
  const counter = page.getByTestId("token-counter");
  await expect(counter).toBeVisible();
  await counter.hover();
  const tokens = page.getByTestId("token-ledger");
  await expect(tokens).toBeVisible();
  await expect(tokens).toHaveAttribute("open", "");
  await expect(tokens.locator("summary")).toContainText(" out · ");
  const table = tokens.getByRole("table", { name: "Tokens by agent" });
  await expect(table.getByRole("columnheader", { name: "Output" })).toBeVisible();
  await expect(table.getByRole("rowheader", { name: "Master (plan)" })).toBeVisible();
  await expect(table.getByRole("rowheader", { name: "Page home" })).toBeVisible();
  const runRow = table.locator("tfoot tr");
  await expect(runRow).toContainText(ledger.totals.output.toLocaleString("en-US"));
  await expect(runRow).toContainText(ledger.totals.cache_read.toLocaleString("en-US"));
  await expect(tokens.getByRole("img", { name: /Whole run: output/ })).toBeVisible();
  await expect(page.getByTestId("agent-split").first()).toContainText(" out · ");
}
