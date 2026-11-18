import { test, expect } from "@playwright/test";
import { lowContrast } from "./fixtures";
import { createServer, type Server } from "node:http";
import { readFileSync, existsSync, statSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// The public site (site/), built from the saved release list and served as plain files.
// This file is the preview release list, not the real one: nothing here reaches GitHub.

const siteDir = resolve(dirname(fileURLToPath(import.meta.url)), "../../site");
const dist = join(siteDir, "dist");
const shots = resolve(siteDir, "../assets/screenshots");
const TYPES: Record<string, string> = { ".html": "text/html", ".css": "text/css", ".js": "text/javascript", ".png": "image/png", ".ico": "image/x-icon", ".json": "application/json" };
let server: Server;
let base = "";

test.beforeAll(async () => {
  execFileSync("node", ["build.mjs"], { cwd: siteDir, env: { ...process.env, TASKMASTER_RELEASES_FILE: join(siteDir, "fixtures/releases.json") } });
  server = createServer((req, res) => {
    let path = join(dist, decodeURIComponent(new URL(req.url!, "http://x").pathname));
    if (existsSync(path) && statSync(path).isDirectory()) path = join(path, "index.html");
    if (!path.startsWith(dist) || !existsSync(path)) return res.writeHead(404).end();
    res.writeHead(200, { "content-type": TYPES[extname(path)] ?? "application/octet-stream" }).end(readFileSync(path));
  });
  await new Promise<void>((ok) => server.listen(0, "127.0.0.1", ok));
  base = `http://127.0.0.1:${(server.address() as { port: number }).port}`;
});
test.afterAll(() => server.close());


for (const theme of ["dark", "light"] as const) {
  test(`landing page, ${theme}: content, contrast, and the theme switch`, async ({ page }) => {
    if (theme === "light") await page.addInitScript(() => localStorage.setItem("taskmaster-site-theme", "light"));
    await page.goto(base + "/");
    await expect(page.getByRole("heading", { level: 1 })).toContainText("One master plans.");
    await expect(page.getByRole("link", { name: /Download/ }).first()).toHaveAttribute("href", "/releases/");
    await expect(page.getByRole("link", { name: "Read the source on GitHub" })).toHaveAttribute("href", "https://github.com/narensen/Taskmaster");
    for (const name of ["Claude Code", "Grok Build", "Antigravity", "Codex", "OpenCode"]) await expect(page.locator(".providers")).toContainText(name);
    expect(await lowContrast(page)).toEqual([]);
    await expect(page.getByRole("button", { name: theme === "dark" ? "Use the light theme" : "Use the dark theme" })).toBeVisible();
    const toggle = page.locator("[data-theme]");
    await toggle.click();
    // Only the theme class is ours to check; the scroll library adds its own.
    if (theme === "dark") await expect(page.locator("html")).toHaveClass(/(^|\s)light(\s|$)/);
    else await expect(page.locator("html")).not.toHaveClass(/(^|\s)light(\s|$)/);
    await toggle.click();
    if (process.env.SHOTS) await page.screenshot({ path: `${shots}/40-site-${theme}.png`, fullPage: true });
  });
}

test("the releases page lists the download for this computer first, with checksums", async ({ browser }) => {
  const linux = await browser.newPage({ userAgent: "Mozilla/5.0 (X11; Linux x86_64) Chrome/140" });
  await linux.goto(base + "/releases/");
  const first = linux.locator("[data-latest] .downloads a").first();
  await expect(first).toHaveAttribute("data-os", "linux");
  await expect(first).toContainText("For this computer");
  await expect(linux.locator("[data-latest]")).not.toContainText(".sig");
  await linux.getByText("SHA-256 checksums").first().click();
  await expect(linux.locator("[data-latest] .sums pre")).toContainText("Taskmaster_0.2.0_amd64.AppImage");
  await expect(linux.getByText("Taskmaster 0.1.0")).toBeVisible();
  expect(await lowContrast(linux)).toEqual([]);
  if (process.env.SHOTS) await linux.screenshot({ path: `${shots}/41-releases-dark.png`, fullPage: true });
  const mac = await browser.newPage({ userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) Safari/605" });
  await mac.goto(base + "/releases/");
  await expect(mac.locator("[data-latest] .downloads a").first()).toHaveAttribute("data-os", "mac-arm");
});

test("at phone width nothing scrolls sideways", async ({ browser }) => {
  const phone = await browser.newPage({ viewport: { width: 375, height: 760 } });
  for (const path of ["/", "/releases/"]) {
    await phone.goto(base + path);
    const overflow = await phone.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(overflow, path).toBeLessThanOrEqual(0);
  }
  if (process.env.SHOTS) {
    await phone.goto(base + "/");
    await phone.screenshot({ path: `${shots}/42-site-phone.png`, fullPage: true });
  }
});
