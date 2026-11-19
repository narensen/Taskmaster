import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect, CHOSEN, finishedRun } from "./fixtures";
import type { Page } from "@playwright/test";

function auth(token: string) {
  return { authorization: `Bearer ${token}`, "content-type": "application/json" };
}

async function restore(url: string, token: string, project: string) {
  await fetch(`${url}/api/projects/open`, { method: "POST", headers: auth(token), body: JSON.stringify({ path: project }) });
}

async function pickIfNeeded(page: Page) {
  const choose = page.getByRole("button", { name: /Choose providers|Choose pages at once/ });
  if (await choose.count()) {
    const first = choose.first();
    if (await first.isVisible()) await first.click();
  }
  await expect(page.getByRole("radiogroup", { name: "Pages at once" })).toBeVisible();
  for (const seat of ["Master", "Mid", "Worker", "Reviewer"]) {
    const button = page.getByRole("button", { name: new RegExp(`^${seat} provider`) });
    if (!(await button.isVisible())) continue;
    const label = (await button.getAttribute("aria-label")) ?? "";
    if (!label.includes("not chosen")) continue;
    await button.click();
    const pop = page.locator(".prov-pop").last();
    await pop.getByRole("listbox", { name: "Providers" }).getByRole("option", { name: /Grok/ }).click();
    await pop.getByRole("option", { name: /Provider default/ }).click();
    await expect(pop).toBeHidden();
  }
  const pages = page.getByRole("radiogroup", { name: "Pages at once" });
  if (await pages.isVisible()) {
    const one = pages.getByRole("radio", { name: "1", exact: true });
    if ((await one.getAttribute("aria-checked")) !== "true") await one.click();
  }
}

test("home offers this project, a new folder, and a clone", async ({ open, page }) => {
  await open("/");
  await expect(page.getByTestId("home-heading")).toBeVisible();
  await expect(page.getByTestId("project-mode-this")).toBeVisible();
  await expect(page.getByTestId("project-mode-new")).toBeVisible();
  await expect(page.getByTestId("project-mode-clone")).toBeVisible();
  await expect(page.locator(".composer-input")).toBeVisible();
});

test("a full scripted run finishes", async ({ openFixture, page, daemon }) => {
  test.setTimeout(90_000);
  const fixture = daemon.fixture!;
  await finishedRun(fixture, "fullrun", "Plan this small product quickly. A one page shop.");
  await openFixture("/runs/fullrun");
  const result = page.getByRole("region", { name: "Run result" });
  await expect(result).toContainText("3 of 3 pages passed");
  await expect(result).toContainText(/Done|Merged into/);
});

test("a usage limit is a sentence with a reset time, a switch, and a retry", async ({ openFixture, page, daemon }) => {
  test.setTimeout(60_000);
  const fixture = daemon.fixture!;
  const headers = auth(fixture.token);
  const made = await fetch(`${fixture.url}/api/runs`, {
    method: "POST",
    headers,
    body: JSON.stringify({ id: "quota", prompt: "USAGE LIMIT", yes: true, roles: CHOSEN }),
  });
  expect([202, 409]).toContain(made.status);
  let sentence = "";
  for (let i = 0; i < 100; i++) {
    const run = (await (await fetch(`${fixture.url}/api/runs/quota`, { headers })).json()) as { error?: string; limit?: { kind?: string } };
    if (run.limit?.kind === "quota") {
      sentence = run.error ?? "";
      break;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  expect(sentence).toContain("Grok hit its usage limit");
  expect(sentence).toContain("Resets 6:30 PM");
  await openFixture("/runs/quota");
  const notice = page.getByTestId("limit");
  await expect(notice).toContainText("Grok hit its usage limit");
  await expect(notice.getByRole("button", { name: "Switch provider for this role" })).toBeVisible();
  await expect(notice.getByRole("button", { name: "Retry" })).toBeVisible();
});

test("a new project is a git repository", async ({ openFixture, page, daemon }) => {
  test.setTimeout(90_000);
  const fixture = daemon.fixture!;
  const before = (await (await fetch(`${fixture.url}/api/settings`, { headers: auth(fixture.token) })).json()) as { project: string };
  const name = `n${Date.now().toString().slice(-8)}`;
  try {
    await openFixture("/");
    await page.getByTestId("project-mode-new").click();
    await page.getByTestId("new-project-name").fill(name);
    await pickIfNeeded(page);
    await page.locator(".composer-input").click();
    await page.keyboard.type("A one page site");
    await page.keyboard.press("Enter");
    await page.waitForURL(/\/runs\/run-/);
    const created = (await (await fetch(`${fixture.url}/api/settings`, { headers: auth(fixture.token) })).json()) as { project: string };
    expect(created.project.endsWith(`/${name}`)).toBe(true);
    execFileSync("git", ["-C", created.project, "rev-parse", "--is-inside-work-tree"]);
    const id = new URL(page.url()).pathname.split("/").pop()!;
    await fetch(`${fixture.url}/api/runs/${id}/cancel`, { method: "POST", headers: auth(fixture.token) });
  } finally {
    await restore(fixture.url, fixture.token, before.project);
  }
});

test("a folder on this machine can be cloned from Home", async ({ open, page, daemon }) => {
  test.setTimeout(60_000);
  const before = (await (await fetch(`${daemon.url}/api/settings`, { headers: auth(daemon.token) })).json()) as { project: string };
  const source = mkdtempSync(join(tmpdir(), "tm-clone-src-"));
  writeFileSync(join(source, "README.md"), "# cloned marker\n");
  execFileSync("git", ["init", "-q", "-b", "main"], { cwd: source });
  execFileSync("git", ["config", "user.email", "e2e@example.invalid"], { cwd: source });
  execFileSync("git", ["config", "user.name", "e2e"], { cwd: source });
  execFileSync("git", ["add", "."], { cwd: source });
  execFileSync("git", ["commit", "-qm", "init"], { cwd: source });
  const folder = `c${Date.now().toString().slice(-8)}`;
  try {
    await open("/");
    await page.getByTestId("project-mode-clone").click();
    await page.getByLabel("Git address").fill(source);
    await page.getByLabel("Folder name").fill(folder);
    await page.getByRole("button", { name: "Clone" }).click();
    await expect
      .poll(async () => {
        const opened = (await (await fetch(`${daemon.url}/api/settings`, { headers: auth(daemon.token) })).json()) as { project: string };
        return opened.project;
      })
      .toContain(`/${folder}`);
    const readme = (await (await fetch(`${daemon.url}/api/file?path=README.md`, { headers: auth(daemon.token) })).json()) as { content?: string };
    expect(readme.content).toContain("cloned marker");
  } finally {
    await restore(daemon.url, daemon.token, before.project);
  }
});

test("settings shows one log, and export, import, and reset keep secrets out", async ({ open, page, daemon }) => {
  await open("/settings");
  const log = page.getByTestId("daemon-log");
  await expect(log).toBeVisible();
  await page.getByRole("button", { name: "Refresh" }).click();
  await expect(log).toContainText("serving");
  const toggle = page.getByTestId("telemetry-setting").getByRole("switch", { name: "Send anonymous counts" });
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  await page.getByRole("button", { name: "Export" }).click();
  const exported = (await (await fetch(`${daemon.url}/api/settings/export`, { headers: auth(daemon.token) })).json()) as {
    settings: { commons?: { token?: string } };
    note: string;
  };
  expect(exported.note).toContain("Tokens are left out");
  expect(exported.settings.commons?.token ?? "").toBe("");
  await page.getByLabel("Settings file to import").setInputFiles({
    name: "taskmaster-settings.json",
    mimeType: "application/json",
    buffer: Buffer.from(JSON.stringify({ telemetry: { enabled: true } })),
  });
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await page.getByRole("button", { name: "Reset choices" }).click();
  await page.getByRole("button", { name: "Click again to reset" }).click();
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  const saved = (await (await fetch(`${daemon.url}/api/settings`, { headers: auth(daemon.token) })).json()) as { telemetry: { enabled: boolean } };
  expect(saved.telemetry.enabled).toBe(false);
});
