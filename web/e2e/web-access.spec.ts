import { test, expect, finishedRun, lowContrast } from "./fixtures";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

test("Settings Web access persists provider, vaulted key, roles, allowlist and RunView displays Web sources", async ({
  open,
  page,
  daemon,
}) => {
  test.setTimeout(90_000);

  // 1. Settings -> Web access: configure provider, vaulted API key, role matrix, and allowlist
  await open("/settings");
  const section = page.getByTestId("web-access-section");
  await expect(section).toBeVisible();

  const enableSwitch = section.getByRole("switch", { name: "Enable agent web access" });
  await expect(enableSwitch).toHaveAttribute("aria-checked", "false");
  await enableSwitch.click();
  await expect(enableSwitch).toHaveAttribute("aria-checked", "true");

  await section.getByTestId("web-access-provider").click();
  await page.locator(".select-pop").getByRole("option", { name: /Brave/ }).click();
  await section.getByTestId("web-access-key").fill("brv-secret-e2e-987654");
  await section.getByTestId("web-access-allowlist").fill("docs.rs, react.dev");
  await section.getByRole("switch", { name: "Worker web search" }).click();
  await section.getByTestId("web-access-save").click();

  await expect(section.getByTestId("web-access-key-saved")).toBeVisible();
  // Secret must NEVER appear anywhere in DOM or input value
  await expect(section.getByTestId("web-access-key")).toHaveValue("");
  await expect(section).not.toContainText("brv-secret-e2e-987654");

  // Settings export must never include the vaulted API key
  const exp = await fetch(`${daemon.url}/api/settings/export`, {
    headers: { authorization: `Bearer ${daemon.token}` },
  });
  expect(exp.ok).toBe(true);
  expect(await exp.text()).not.toContain("brv-secret-e2e-987654");

  // Reload and confirm persistence without leaking secret
  await page.reload();
  const reloaded = page.getByTestId("web-access-section");
  await expect(reloaded.getByRole("switch", { name: "Enable agent web access" })).toHaveAttribute("aria-checked", "true");
  await expect(reloaded.getByTestId("web-access-provider")).toHaveAttribute("data-value", "brave");
  await expect(reloaded.getByTestId("web-access-allowlist")).toHaveValue("docs.rs, react.dev");
  await expect(reloaded.getByRole("switch", { name: "Worker web search" })).toHaveAttribute("aria-checked", "true");
  await expect(reloaded.getByTestId("web-access-key-saved")).toBeVisible();
  await expect(reloaded).not.toContainText("brv-secret-e2e-987654");

  // Clear cache works cleanly
  await reloaded.getByTestId("web-access-clear-cache").click();
  await expect(reloaded.getByTestId("web-access-cache-stats")).toContainText("Cache: 0 pages");

  // Contrast check on Settings with Web access panel visible
  expect(await lowContrast(page)).toEqual([]);

  // Clean up test key & reset toggle so later tests see clean defaults
  await reloaded.getByRole("button", { name: "Clear key" }).click();
  await expect(reloaded.getByTestId("web-access-key-saved")).toHaveCount(0);
  await reloaded.getByRole("switch", { name: "Enable agent web access" }).click();
  await reloaded.getByTestId("web-access-save").click();

  // 2. RunView -> Web sources: real daemon serves /api/runs/<id>/web and plan.research
  await finishedRun(daemon.fixture!, "web-sources-seed", "Build a small API server.");
  const seedDir = join(dirname(daemon.project), "fixture/.taskmaster/runs/web-sources-seed");
  const targetDir = join(daemon.project, ".taskmaster/runs/web-sources-e2e");
  mkdirSync(targetDir, { recursive: true });

  // Copy real status.json & enrich plan.json + events.jsonl with planner research notes
  const research = [
    {
      decision: "Use axum 0.8 path syntax with curly braces",
      source: "https://docs.rs/axum/0.8",
      version: "0.8.4",
    },
  ];
  writeFileSync(join(targetDir, "status.json"), readFileSync(join(seedDir, "status.json")));
  const patchedEvents = readFileSync(join(seedDir, "events.jsonl"), "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const ev = JSON.parse(line);
      if (ev.plan_ready) ev.plan_ready.research = research;
      if (ev.event?.plan_ready) ev.event.plan_ready.research = research;
      if (ev.plan) ev.plan.research = research;
      return JSON.stringify(ev);
    })
    .join("\n") + "\n";
  writeFileSync(join(targetDir, "events.jsonl"), patchedEvents);
  const plan = JSON.parse(readFileSync(join(seedDir, "plan.json"), "utf8"));
  plan.research = research;
  writeFileSync(join(targetDir, "plan.json"), JSON.stringify(plan, null, 2));

  // Write real web_search, web_fetch (cached + injection flagged), and local docs transcript entries
  const webEntries = [
    {
      at: "2026-11-22T10:00:01Z",
      agent: { id: "master", role: "master" },
      kind: "web_search",
      provider: "searxng",
      query: "axum 0.8 router path syntax",
      hits: 1,
      sources: [{ title: "axum::Router docs", url: "https://docs.rs/axum/0.8/axum/struct.Router.html", engine: "searxng" }],
      injection_flagged: false,
    },
    {
      at: "2026-11-22T10:00:02Z",
      agent: { id: "page:api", role: "worker", page: "api" },
      kind: "web_fetch",
      url: "https://docs.rs/axum/0.8/axum/struct.Router.html",
      final_url: "https://docs.rs/axum/0.8/axum/struct.Router.html",
      host: "docs.rs",
      bytes: 4096,
      cached: true,
      injection_flagged: true,
    },
    {
      at: "2026-11-22T10:00:03Z",
      agent: { id: "page:api", role: "worker", page: "api" },
      kind: "docs",
      package: "react",
      version: "19.0.0",
      ecosystem: "npm",
      symbol: "useActionState",
      source: "local",
      path: "node_modules/react/index.d.ts",
    },
    {
      at: "2026-11-22T10:00:04Z",
      agent: { id: "reviewer", role: "reviewer" },
      kind: "advisories",
      ecosystem: "npm",
      package: "semver",
      version: "7.5.1",
      count: 1,
      advisories: [{ id: "GHSA-c2qf-rxjj-qqgw", summary: "ReDoS in semver range parsing" }],
      url: "https://api.osv.dev/v1/query",
      injection_flagged: false,
    },
  ];
  writeFileSync(join(targetDir, "agents.jsonl"), webEntries.map((e) => JSON.stringify(e)).join("\n") + "\n");

  await open("/runs/web-sources-e2e");
  const sources = page.getByTestId("web-sources");
  await expect(sources).toBeVisible();
  await expect(sources).toContainText("Use axum 0.8 path syntax with curly braces");
  await expect(sources).toContainText("0.8.4");
  await expect(sources).toContainText("axum 0.8 router path syntax");
  await expect(sources).toContainText("axum::Router docs");
  await expect(sources).toContainText("https://docs.rs/axum/0.8/axum/struct.Router.html");
  await expect(sources).toContainText("cached");
  await expect(sources).toContainText("untrusted prompt pattern flagged");
  await expect(sources).toContainText("react@19.0.0");
  await expect(sources).toContainText("useActionState");
  await expect(sources).toContainText("GHSA-c2qf-rxjj-qqgw");
  await expect(sources).toContainText("semver@7.5.1");
});
